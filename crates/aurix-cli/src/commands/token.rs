use anyhow::bail;
use clap::{Args as ClapArgs, Subcommand};
use serde_json::{json, Value};

use super::{b, f, i, obj, query, s, Ctx};
use crate::http::Auth;
use crate::output::json_arg;

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Issue a player session token (`POST /v1/tokens`). Keep this on your backend.
    Issue {
        /// Your stable player id (account id); the user is created on first use.
        #[arg(long)]
        external_id: String,
        #[arg(long)]
        display_name: String,
        /// Channel grants: `CHANNEL_ID` or `CHANNEL_ID:flags` where flags ⊂ {j,s,r,m,p} (join/speak/receive/moderate/priority; default jsr).
        #[arg(long = "channel", value_name = "ID[:FLAGS]")]
        channels: Vec<String>,
        /// Grant an ad-hoc channel (created on first join): `NAME[:TYPE[:MAX]]`, TYPE ∈ {team,positional,command,whisper,echo} (default team);
        /// flags as in `--channel` after `@`, e.g. `raid-7:team:40@jsrm`.
        #[arg(long, value_name = "NAME[:TYPE[:MAX]][@FLAGS]")]
        ad_hoc: Vec<String>,
        /// Preferred region for the returned endpoint.
        #[arg(long, value_parser = crate::openapi::enum_parser("Region"))]
        region: Option<String>,
        /// Player latitude/longitude for nearest-node selection.
        #[arg(long, requires = "longitude")]
        latitude: Option<f64>,
        #[arg(long, requires = "latitude")]
        longitude: Option<f64>,
        /// Opaque JSON metadata attached to the user.
        #[arg(long, value_name = "JSON|@file")]
        metadata: Option<String>,
    },
    /// Issue a one-shot action token (`POST /v1/tokens/action`).
    Action {
        #[arg(long, value_parser = ["login", "join", "kick", "mute", "unmute"])]
        action: String,
        #[arg(long, conflicts_with = "external_id")]
        user_id: Option<String>,
        #[arg(long, requires = "display_name")]
        external_id: Option<String>,
        #[arg(long)]
        display_name: Option<String>,
        #[arg(long)]
        channel_id: Option<String>,
        #[arg(long)]
        target_user_id: Option<String>,
        #[arg(long)]
        speak: Option<bool>,
        #[arg(long)]
        receive: Option<bool>,
        #[arg(long)]
        moderate: Option<bool>,
        #[arg(long)]
        priority: Option<bool>,
        /// `join` only: create the channel on first join — `NAME[:TYPE[:MAX]]`.
        #[arg(long, value_name = "NAME[:TYPE[:MAX]]", conflicts_with = "channel_id")]
        ad_hoc: Option<String>,
        #[arg(long)]
        ttl_secs: Option<i64>,
        #[arg(long, value_name = "JSON|@file")]
        metadata: Option<String>,
    },
}

/// `NAME[:TYPE[:MAX]]` → `AdHocChannel`.
pub fn parse_ad_hoc(spec: &str) -> anyhow::Result<Value> {
    let mut parts = spec.splitn(3, ':');
    let name = parts.next().unwrap_or_default();
    if name.is_empty() {
        bail!("ad-hoc channel name is empty");
    }
    let channel_type = parts.next().filter(|t| !t.is_empty()).unwrap_or("team");
    if !["positional", "team", "command", "whisper", "echo"].contains(&channel_type) {
        bail!("unknown channel type '{channel_type}' (positional|team|command|whisper|echo)");
    }
    let max: Option<i64> = parts
        .next()
        .map(|m| {
            m.parse()
                .map_err(|_| anyhow::anyhow!("bad max participants '{m}'"))
        })
        .transpose()?;
    Ok(obj(vec![
        ("name", Some(Value::String(name.to_string()))),
        (
            "channel_type",
            Some(Value::String(channel_type.to_string())),
        ),
        ("max_participants", i(&max)),
    ]))
}

/// `NAME[:TYPE[:MAX]][@FLAGS]` → `ChannelGrant` with `ad_hoc` set and no `channel_id`.
pub fn parse_ad_hoc_grant(spec: &str) -> anyhow::Result<Value> {
    let (chan, flags) = spec.split_once('@').unwrap_or((spec, "jsr"));
    let mut g = parse_grant(&format!("_:{flags}"))?;
    if let Some(o) = g.as_object_mut() {
        o.remove("channel_id");
        o.insert("ad_hoc".into(), parse_ad_hoc(chan)?);
    }
    Ok(g)
}

pub fn parse_grant(spec: &str) -> anyhow::Result<Value> {
    let (id, flags) = match spec.split_once(':') {
        Some((id, flags)) => (id, flags),
        None => (spec, "jsr"),
    };
    if id.is_empty() {
        bail!("empty channel id in --channel '{spec}'");
    }
    let mut g = json!({ "channel_id": id, "join": false, "speak": false, "receive": false, "moderate": false, "priority": false });
    for c in flags.chars() {
        let key = match c {
            'j' => "join",
            's' => "speak",
            'r' => "receive",
            'm' => "moderate",
            'p' => "priority",
            _ => bail!("unknown grant flag '{c}' in --channel '{spec}' (use j,s,r,m,p)"),
        };
        g[key] = Value::Bool(true);
    }
    Ok(g)
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<()> {
    match args.cmd {
        Cmd::Issue {
            external_id,
            display_name,
            channels,
            ad_hoc,
            region,
            latitude,
            longitude,
            metadata,
        } => {
            let mut grants = channels
                .iter()
                .map(|c| parse_grant(c))
                .collect::<anyhow::Result<Vec<_>>>()?;
            for spec in &ad_hoc {
                grants.push(parse_ad_hoc_grant(spec)?);
            }
            let body = obj(vec![
                ("external_id", Some(Value::String(external_id))),
                ("display_name", Some(Value::String(display_name))),
                (
                    "channels",
                    (!grants.is_empty()).then_some(Value::Array(grants)),
                ),
                ("region", s(&region)),
                (
                    "location",
                    latitude
                        .zip(longitude)
                        .map(|(lat, lon)| json!({ "latitude": lat, "longitude": lon })),
                ),
                ("metadata", metadata.as_deref().map(json_arg).transpose()?),
            ]);
            ctx.print(&ctx.api.post("/v1/tokens", body, Auth::ApiKey).await?)
        }
        Cmd::Action {
            action,
            user_id,
            external_id,
            display_name,
            channel_id,
            target_user_id,
            speak,
            receive,
            moderate,
            priority,
            ad_hoc,
            ttl_secs,
            metadata,
        } => {
            if user_id.is_none() && external_id.is_none() {
                bail!("pass --user-id or --external-id + --display-name");
            }
            let body = obj(vec![
                ("action", Some(Value::String(action))),
                ("user_id", s(&user_id)),
                ("external_id", s(&external_id)),
                ("display_name", s(&display_name)),
                ("channel_id", s(&channel_id)),
                ("target_user_id", s(&target_user_id)),
                ("speak", b(speak)),
                ("receive", b(receive)),
                ("moderate", b(moderate)),
                ("priority", b(priority)),
                ("ad_hoc", ad_hoc.as_deref().map(parse_ad_hoc).transpose()?),
                ("ttl_secs", i(&ttl_secs)),
                ("metadata", metadata.as_deref().map(json_arg).transpose()?),
            ]);
            ctx.print(
                &ctx.api
                    .post("/v1/tokens/action", body, Auth::ApiKey)
                    .await?,
            )
        }
    }
}

#[derive(ClapArgs)]
pub struct TurnArgs {
    #[command(subcommand)]
    cmd: TurnCmd,
}

#[derive(Subcommand)]
enum TurnCmd {
    /// Short-lived TURN credentials for a player (`POST /v1/turn/credentials`).
    Credentials {
        #[arg(long)]
        user_id: String,
    },
}

pub async fn turn(ctx: &Ctx, args: TurnArgs) -> anyhow::Result<()> {
    match args.cmd {
        TurnCmd::Credentials { user_id } => ctx.print(
            &ctx.api
                .post(
                    "/v1/turn/credentials",
                    json!({ "user_id": user_id }),
                    Auth::ApiKey,
                )
                .await?,
        ),
    }
}

#[derive(ClapArgs)]
pub struct RegionsArgs {
    /// Only this region.
    #[arg(long, value_parser = crate::openapi::enum_parser("Region"))]
    region: Option<String>,
    /// Rank by distance from these coordinates.
    #[arg(long, requires = "longitude")]
    latitude: Option<f64>,
    #[arg(long, requires = "latitude")]
    longitude: Option<f64>,
}

pub async fn regions(ctx: &Ctx, args: RegionsArgs) -> anyhow::Result<()> {
    let q = query(vec![
        ("region", args.region),
        ("latitude", f(&args.latitude).map(|v| v.to_string())),
        ("longitude", f(&args.longitude).map(|v| v.to_string())),
    ]);
    ctx.print(&ctx.api.get("/v1/regions", &q, Auth::ApiKey).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_parse_flags() {
        let g = parse_grant("abc").unwrap();
        assert_eq!(g["channel_id"], "abc");
        assert_eq!(
            (
                g["join"].as_bool(),
                g["speak"].as_bool(),
                g["receive"].as_bool()
            ),
            (Some(true), Some(true), Some(true))
        );
        assert_eq!(g["moderate"], false);
        let g = parse_grant("abc:rm").unwrap();
        assert_eq!(
            (&g["join"], &g["receive"], &g["moderate"], &g["speak"]),
            (&json!(false), &json!(true), &json!(true), &json!(false))
        );
        assert!(parse_grant("abc:x").is_err());
        assert!(parse_grant(":r").is_err());
    }

    #[test]
    fn ad_hoc_grants_follow_schema() {
        let g = parse_ad_hoc_grant("raid-7").unwrap();
        assert!(g.get("channel_id").is_none());
        assert_eq!(
            g["ad_hoc"],
            json!({ "name": "raid-7", "channel_type": "team" })
        );
        assert_eq!(
            (&g["join"], &g["speak"], &g["receive"]),
            (&json!(true), &json!(true), &json!(true))
        );
        let g = parse_ad_hoc_grant("zone:positional:40@jrm").unwrap();
        assert_eq!(
            g["ad_hoc"],
            json!({ "name": "zone", "channel_type": "positional", "max_participants": 40 })
        );
        assert_eq!((&g["speak"], &g["moderate"]), (&json!(false), &json!(true)));
        assert!(parse_ad_hoc("x:video").is_err());
        assert!(parse_ad_hoc(":team").is_err());
        assert!(parse_ad_hoc("x:team:many").is_err());
    }
}
