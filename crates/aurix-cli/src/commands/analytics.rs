use std::path::PathBuf;

use anyhow::bail;
use clap::{Args as ClapArgs, Subcommand};
use reqwest::Method;
use serde_json::{json, Value};

use super::{obj, query, s, seg, Ctx};
use crate::http::{Auth, SseEvent};

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Totals + time series for the app (CCU, minutes, MOS).
    Summary {
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        /// Bucket size, e.g. `5m`, `1h`, `1d`.
        #[arg(long)]
        step: Option<String>,
    },
    /// Per-app quota and current consumption.
    Quota,
    /// Worst sessions by MOS.
    Sessions {
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        limit: Option<i64>,
        #[arg(long)]
        min_samples: Option<i64>,
    },
    /// Usage per channel.
    Channels {
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        limit: Option<i64>,
    },
    Channel {
        channel_id: String,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
    },
    /// Export usage rows as JSON or CSV.
    Export {
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long, value_parser = ["app", "channels"])]
        scope: Option<String>,
        #[arg(long, default_value = "json", value_parser = ["json", "csv"])]
        format: String,
        /// Write to a file instead of stdout.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<()> {
    let api = &ctx.api;
    let a = Auth::ApiKey;
    match args.cmd {
        Cmd::Summary { from, to, step } => {
            let q = query(vec![("from", from), ("to", to), ("step", step)]);
            ctx.print(&api.get("/v1/analytics", &q, a).await?)
        }
        Cmd::Quota => ctx.print(&api.get("/v1/analytics/quota", &[], a).await?),
        Cmd::Sessions {
            from,
            to,
            limit,
            min_samples,
        } => {
            let q = query(vec![
                ("from", from),
                ("to", to),
                ("limit", limit.map(|v| v.to_string())),
                ("min_samples", min_samples.map(|v| v.to_string())),
            ]);
            ctx.print(&api.get("/v1/analytics/sessions", &q, a).await?)
        }
        Cmd::Channels { from, to, limit } => {
            let q = query(vec![
                ("from", from),
                ("to", to),
                ("limit", limit.map(|v| v.to_string())),
            ]);
            ctx.print(&api.get("/v1/analytics/channels", &q, a).await?)
        }
        Cmd::Channel {
            channel_id,
            from,
            to,
        } => {
            let q = query(vec![("from", from), ("to", to)]);
            ctx.print(
                &api.get(
                    &format!("/v1/analytics/channels/{}", seg(&channel_id)),
                    &q,
                    a,
                )
                .await?,
            )
        }
        Cmd::Export {
            from,
            to,
            scope,
            format,
            out,
        } => {
            let q = query(vec![
                ("from", from),
                ("to", to),
                ("scope", scope),
                ("format", Some(format.clone())),
            ]);
            let resp = api
                .send(Method::GET, "/v1/analytics/export", &q, None, a)
                .await?;
            write_export(ctx, resp.body, &resp.content_type, format == "json", out)
        }
    }
}

pub fn write_export(
    ctx: &Ctx,
    body: Vec<u8>,
    content_type: &str,
    is_json: bool,
    out: Option<PathBuf>,
) -> anyhow::Result<()> {
    match out {
        Some(path) => {
            std::fs::write(&path, &body)?;
            ctx.print(
                &json!({ "written": path, "bytes": body.len(), "content_type": content_type }),
            )
        }
        None if is_json => {
            let v: Value = serde_json::from_slice(&body)?;
            ctx.print(&v)
        }
        None => {
            use std::io::Write;
            std::io::stdout().write_all(&body)?;
            Ok(())
        }
    }
}

#[derive(ClapArgs)]
pub struct EventsArgs {
    #[command(subcommand)]
    cmd: EventsCmd,
}

#[derive(Subcommand)]
enum EventsCmd {
    /// Follow `GET /v1/events` (SSE); one JSON event per line. Ctrl-C to stop.
    Tail {
        /// Only these event types (comma-separated), e.g. `participant.joined,quality.alert`.
        #[arg(long, value_delimiter = ',')]
        types: Vec<String>,
        /// Resume after this event id.
        #[arg(long)]
        last_event_id: Option<String>,
        /// Exit after N events.
        #[arg(long)]
        count: Option<usize>,
        /// Do not reconnect when the stream ends.
        #[arg(long)]
        no_reconnect: bool,
        /// Also print stream control events (`stream.open`, heartbeats).
        #[arg(long)]
        all: bool,
    },
    /// Current roster/state snapshot for consumers that resync.
    Snapshot,
}

pub async fn events(ctx: &Ctx, args: EventsArgs) -> anyhow::Result<()> {
    match args.cmd {
        EventsCmd::Snapshot => ctx.print(
            &ctx.api
                .get("/v1/events/snapshot", &[], Auth::ApiKey)
                .await?,
        ),
        EventsCmd::Tail {
            types,
            last_event_id,
            count,
            no_reconnect,
            all,
        } => {
            let q = query(vec![(
                "types",
                (!types.is_empty()).then(|| types.join(",")),
            )]);
            let mut last = last_event_id;
            let seen = std::cell::Cell::new(0usize);
            let done = std::cell::Cell::new(false);
            let mut delay = std::time::Duration::from_secs(1);
            loop {
                let mut on_event = |ev: SseEvent| -> bool {
                    let control = ev.event.starts_with("stream.")
                        || ev.event == "heartbeat"
                        || ev.event == "message";
                    if control && !all {
                        return true;
                    }
                    let data: Value =
                        serde_json::from_str(&ev.data).unwrap_or(Value::String(ev.data.clone()));
                    let line = obj(vec![
                        ("id", s(&ev.id)),
                        ("event", Some(Value::String(ev.event.clone()))),
                        ("data", Some(data)),
                    ]);
                    if ctx.print(&line).is_err() {
                        done.set(true);
                        return false;
                    }
                    seen.set(seen.get() + 1);
                    if count.is_some_and(|c| seen.get() >= c) {
                        done.set(true);
                        return false;
                    }
                    true
                };
                let res = ctx
                    .api
                    .sse(
                        "/v1/events",
                        &q,
                        last.as_deref(),
                        Auth::ApiKey,
                        &mut on_event,
                    )
                    .await;
                match res {
                    Ok(id) => {
                        if let Some(id) = id {
                            last = Some(id);
                        }
                        delay = std::time::Duration::from_secs(1);
                    }
                    Err(e) => {
                        if e.downcast_ref::<crate::http::ApiError>().is_some() {
                            return Err(e);
                        }
                        eprintln!("stream error: {e:#}");
                    }
                }
                if done.get() || no_reconnect {
                    return Ok(());
                }
                eprintln!("reconnecting in {}s…", delay.as_secs());
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_secs(30));
            }
        }
    }
}

#[derive(ClapArgs)]
pub struct RecordingArgs {
    #[command(subcommand)]
    cmd: RecordingCmd,
}

#[derive(Subcommand)]
enum RecordingCmd {
    List {
        #[arg(long)]
        channel_id: Option<String>,
        #[arg(long)]
        page: Option<i64>,
        #[arg(long)]
        per_page: Option<i64>,
    },
    Get {
        recording_id: String,
    },
    /// Start recording one participant (consent gating applies).
    Start {
        #[arg(long)]
        channel_id: String,
        #[arg(long)]
        user_id: String,
        #[arg(long)]
        session_id: Option<String>,
    },
    Stop {
        recording_id: String,
    },
    Delete {
        recording_id: String,
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Download the media file (`--format wav` for a decoded WAV).
    Download {
        recording_id: String,
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
        #[arg(long, value_parser = ["ogg_opus", "wav"])]
        format: Option<String>,
    },
    /// Combine finished tracks of a channel into one file.
    Mixdown {
        #[arg(long)]
        channel_id: String,
        #[arg(long = "source")]
        sources: Vec<String>,
        #[arg(long, value_parser = ["ogg_opus", "wav"])]
        format: Option<String>,
        #[arg(long)]
        stereo: bool,
    },
    /// Queue post-hoc speech-to-text of a recording.
    Transcribe {
        recording_id: String,
    },
    /// Fetch the transcript (`json`, `srt`, `vtt`).
    Transcript {
        recording_id: String,
        #[arg(long, default_value = "json", value_parser = ["json", "srt", "vtt"])]
        format: String,
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
}

pub async fn recordings(ctx: &Ctx, args: RecordingArgs) -> anyhow::Result<()> {
    let api = &ctx.api;
    let a = Auth::ApiKey;
    match args.cmd {
        RecordingCmd::List {
            channel_id,
            page,
            per_page,
        } => {
            let q = query(vec![
                ("channel_id", channel_id),
                ("page", page.map(|v| v.to_string())),
                ("per_page", per_page.map(|v| v.to_string())),
            ]);
            ctx.print(&api.get("/v1/recordings", &q, a).await?)
        }
        RecordingCmd::Get { recording_id } => ctx.print(
            &api.get(&format!("/v1/recordings/{}", seg(&recording_id)), &[], a)
                .await?,
        ),
        RecordingCmd::Start {
            channel_id,
            user_id,
            session_id,
        } => {
            let body = obj(vec![
                ("channel_id", Some(Value::String(channel_id))),
                ("user_id", Some(Value::String(user_id))),
                ("session_id", s(&session_id)),
            ]);
            ctx.print(&api.post("/v1/recordings/start", body, a).await?)
        }
        RecordingCmd::Stop { recording_id } => ctx.print(
            &api.post(
                &format!("/v1/recordings/{}/stop", seg(&recording_id)),
                json!({}),
                a,
            )
            .await?,
        ),
        RecordingCmd::Delete { recording_id, yes } => {
            if !yes {
                bail!("this deletes the media file; re-run with --yes");
            }
            ctx.print(
                &api.delete(&format!("/v1/recordings/{}", seg(&recording_id)), &[], a)
                    .await?,
            )
        }
        RecordingCmd::Download {
            recording_id,
            out,
            format,
        } => {
            let q = query(vec![("format", format)]);
            let resp = api
                .send(
                    Method::GET,
                    &format!("/v1/recordings/{}/download", seg(&recording_id)),
                    &q,
                    None,
                    a,
                )
                .await?;
            std::fs::write(&out, &resp.body)?;
            ctx.print(&json!({ "written": out, "bytes": resp.body.len(), "content_type": resp.content_type }))
        }
        RecordingCmd::Mixdown {
            channel_id,
            sources,
            format,
            stereo,
        } => {
            let body = obj(vec![
                ("channel_id", Some(Value::String(channel_id))),
                (
                    "sources",
                    (!sources.is_empty())
                        .then(|| Value::Array(sources.into_iter().map(Value::String).collect())),
                ),
                ("format", s(&format)),
                ("stereo", stereo.then_some(Value::Bool(true))),
            ]);
            ctx.print(&api.post("/v1/recordings/mixdown", body, a).await?)
        }
        RecordingCmd::Transcribe { recording_id } => ctx.print(
            &api.post(
                &format!("/v1/recordings/{}/transcribe", seg(&recording_id)),
                json!({}),
                a,
            )
            .await?,
        ),
        RecordingCmd::Transcript {
            recording_id,
            format,
            out,
        } => {
            let q = query(vec![("format", Some(format.clone()))]);
            let resp = api
                .send(
                    Method::GET,
                    &format!("/v1/recordings/{}/transcript", seg(&recording_id)),
                    &q,
                    None,
                    a,
                )
                .await?;
            write_export(ctx, resp.body, &resp.content_type, format == "json", out)
        }
    }
}
