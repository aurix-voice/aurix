use clap::{Args as ClapArgs, Subcommand};
use serde_json::Value;

use super::{i, obj, query, s, seg, Ctx};
use crate::http::Auth;
use crate::output::json_arg;

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Ban a user (account, device or IP scope).
    Ban {
        #[arg(long)]
        user_id: String,
        #[arg(long)]
        reason: String,
        #[arg(long, default_value = "account", value_parser = ["account", "device", "ip_address"])]
        scope: String,
        /// Omit for a permanent ban.
        #[arg(long)]
        duration_hours: Option<i64>,
        #[arg(long)]
        device_id: Option<String>,
        #[arg(long)]
        ip_address: Option<String>,
        /// Attributed moderator user id.
        #[arg(long)]
        moderator: Option<String>,
    },
    /// List bans.
    Bans {
        #[arg(long)]
        user_id: Option<String>,
        #[arg(long)]
        page: Option<i64>,
        #[arg(long)]
        per_page: Option<i64>,
    },
    /// Revoke one ban by id.
    RevokeBan {
        ban_id: String,
        #[arg(long)]
        moderator: Option<String>,
    },
    /// Lift every active ban of a user.
    Unban {
        user_id: String,
        #[arg(long)]
        moderator: Option<String>,
    },
    /// Server-side mute in a channel (`--off` to unmute).
    Mute {
        #[arg(long)]
        user_id: String,
        #[arg(long)]
        channel_id: String,
        #[arg(long)]
        off: bool,
        #[arg(long)]
        moderator: Option<String>,
    },
    /// Mute everyone in a channel (`--off` to unmute).
    MuteAll {
        #[arg(long)]
        channel_id: String,
        #[arg(long)]
        off: bool,
        /// User ids to leave untouched.
        #[arg(long = "except")]
        except: Vec<String>,
        #[arg(long)]
        moderator: Option<String>,
    },
    /// Priority speaker flag (channel must have `ducking` configured; `--off` to clear).
    Priority {
        #[arg(long)]
        user_id: String,
        #[arg(long)]
        channel_id: String,
        #[arg(long)]
        off: bool,
        #[arg(long)]
        moderator: Option<String>,
    },
    Kick {
        #[arg(long)]
        user_id: String,
        #[arg(long)]
        channel_id: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        moderator: Option<String>,
    },
    KickAll {
        #[arg(long)]
        channel_id: String,
        #[arg(long)]
        reason: String,
        #[arg(long = "except")]
        except: Vec<String>,
        #[arg(long)]
        moderator: Option<String>,
    },
    /// File a report on behalf of a player.
    Report {
        #[arg(long)]
        target_user_id: String,
        #[arg(long)]
        reporter_user_id: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        channel_id: Option<String>,
        #[arg(long)]
        recording_id: Option<String>,
        #[arg(long, value_name = "JSON")]
        evidence: Option<String>,
    },
    /// Moderation events (reports, auto-actions).
    Events {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        page: Option<i64>,
        #[arg(long)]
        per_page: Option<i64>,
    },
    Event {
        event_id: String,
    },
    Resolve {
        event_id: String,
        #[arg(long)]
        resolution: String,
        #[arg(long)]
        moderator: Option<String>,
    },
    /// Safety incidents (voice/chat classifier).
    Incidents {
        #[arg(long)]
        user_id: Option<String>,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        page: Option<i64>,
        #[arg(long)]
        per_page: Option<i64>,
    },
    Incident {
        incident_id: String,
        /// Fetch the evidence bundle instead of the incident record.
        #[arg(long)]
        export: bool,
    },
    /// Accumulated safety risk score of a user.
    Risk {
        user_id: String,
    },
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<()> {
    let api = &ctx.api;
    let a = Auth::ApiKey;
    match args.cmd {
        Cmd::Ban {
            user_id,
            reason,
            scope,
            duration_hours,
            device_id,
            ip_address,
            moderator,
        } => {
            let body = obj(vec![
                ("user_id", Some(Value::String(user_id))),
                ("scope", Some(Value::String(scope))),
                ("reason", Some(Value::String(reason))),
                ("duration_hours", i(&duration_hours)),
                ("device_id", s(&device_id)),
                ("ip_address", s(&ip_address)),
                ("moderator_user_id", s(&moderator)),
            ]);
            ctx.print(&api.post("/v1/moderation/ban", body, a).await?)
        }
        Cmd::Bans {
            user_id,
            page,
            per_page,
        } => {
            let q = query(vec![
                ("user_id", user_id),
                ("page", page.map(|v| v.to_string())),
                ("per_page", per_page.map(|v| v.to_string())),
            ]);
            ctx.print(&api.get("/v1/moderation/bans", &q, a).await?)
        }
        Cmd::RevokeBan { ban_id, moderator } => {
            let body = obj(vec![("moderator_user_id", s(&moderator))]);
            ctx.print(
                &api.post(
                    &format!("/v1/moderation/bans/{}/revoke", seg(&ban_id)),
                    body,
                    a,
                )
                .await?,
            )
        }
        Cmd::Unban { user_id, moderator } => {
            let body = obj(vec![("moderator_user_id", s(&moderator))]);
            ctx.print(
                &api.post(&format!("/v1/users/{}/unban", seg(&user_id)), body, a)
                    .await?,
            )
        }
        Cmd::Mute {
            user_id,
            channel_id,
            off,
            moderator,
        } => {
            let body = obj(vec![
                ("user_id", Some(Value::String(user_id))),
                ("channel_id", Some(Value::String(channel_id))),
                ("muted", Some(Value::Bool(!off))),
                ("moderator_user_id", s(&moderator)),
            ]);
            ctx.print(&api.post("/v1/moderation/mute", body, a).await?)
        }
        Cmd::MuteAll {
            channel_id,
            off,
            except,
            moderator,
        } => {
            let body = obj(vec![
                ("channel_id", Some(Value::String(channel_id))),
                ("muted", Some(Value::Bool(!off))),
                (
                    "except",
                    (!except.is_empty())
                        .then(|| Value::Array(except.into_iter().map(Value::String).collect())),
                ),
                ("moderator_user_id", s(&moderator)),
            ]);
            ctx.print(&api.post("/v1/moderation/mute-all", body, a).await?)
        }
        Cmd::Priority {
            user_id,
            channel_id,
            off,
            moderator,
        } => {
            let body = obj(vec![
                ("user_id", Some(Value::String(user_id))),
                ("channel_id", Some(Value::String(channel_id))),
                ("priority", Some(Value::Bool(!off))),
                ("moderator_user_id", s(&moderator)),
            ]);
            ctx.print(&api.post("/v1/moderation/priority", body, a).await?)
        }
        Cmd::Kick {
            user_id,
            channel_id,
            reason,
            moderator,
        } => {
            let body = obj(vec![
                ("user_id", Some(Value::String(user_id))),
                ("channel_id", Some(Value::String(channel_id))),
                ("reason", Some(Value::String(reason))),
                ("moderator_user_id", s(&moderator)),
            ]);
            ctx.print(&api.post("/v1/moderation/kick", body, a).await?)
        }
        Cmd::KickAll {
            channel_id,
            reason,
            except,
            moderator,
        } => {
            let body = obj(vec![
                ("channel_id", Some(Value::String(channel_id))),
                ("reason", Some(Value::String(reason))),
                (
                    "except",
                    (!except.is_empty())
                        .then(|| Value::Array(except.into_iter().map(Value::String).collect())),
                ),
                ("moderator_user_id", s(&moderator)),
            ]);
            ctx.print(&api.post("/v1/moderation/kick-all", body, a).await?)
        }
        Cmd::Report {
            target_user_id,
            reporter_user_id,
            reason,
            channel_id,
            recording_id,
            evidence,
        } => {
            let body = obj(vec![
                ("target_user_id", Some(Value::String(target_user_id))),
                ("reporter_user_id", Some(Value::String(reporter_user_id))),
                ("reason", Some(Value::String(reason))),
                ("channel_id", s(&channel_id)),
                ("recording_id", s(&recording_id)),
                ("evidence", evidence.as_deref().map(json_arg).transpose()?),
            ]);
            ctx.print(&api.post("/v1/moderation/report", body, a).await?)
        }
        Cmd::Events {
            status,
            page,
            per_page,
        } => {
            let q = query(vec![
                ("status", status),
                ("page", page.map(|v| v.to_string())),
                ("per_page", per_page.map(|v| v.to_string())),
            ]);
            ctx.print(&api.get("/v1/moderation/events", &q, a).await?)
        }
        Cmd::Event { event_id } => ctx.print(
            &api.get(&format!("/v1/moderation/events/{}", seg(&event_id)), &[], a)
                .await?,
        ),
        Cmd::Resolve {
            event_id,
            resolution,
            moderator,
        } => {
            let body = obj(vec![
                ("resolution", Some(Value::String(resolution))),
                ("moderator_user_id", s(&moderator)),
            ]);
            ctx.print(
                &api.post(
                    &format!("/v1/moderation/events/{}/resolve", seg(&event_id)),
                    body,
                    a,
                )
                .await?,
            )
        }
        Cmd::Incidents {
            user_id,
            source,
            status,
            page,
            per_page,
        } => {
            let q = query(vec![
                ("user_id", user_id),
                ("source", source),
                ("status", status),
                ("page", page.map(|v| v.to_string())),
                ("per_page", per_page.map(|v| v.to_string())),
            ]);
            ctx.print(&api.get("/v1/safety/incidents", &q, a).await?)
        }
        Cmd::Incident {
            incident_id,
            export,
        } => {
            let suffix = if export { "/export" } else { "" };
            ctx.print(
                &api.get(
                    &format!("/v1/safety/incidents/{}{suffix}", seg(&incident_id)),
                    &[],
                    a,
                )
                .await?,
            )
        }
        Cmd::Risk { user_id } => ctx.print(
            &api.get(&format!("/v1/safety/users/{}/risk", seg(&user_id)), &[], a)
                .await?,
        ),
    }
}
