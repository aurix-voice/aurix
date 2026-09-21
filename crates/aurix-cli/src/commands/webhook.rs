use std::path::PathBuf;

use anyhow::{anyhow, bail};
use clap::{Args as ClapArgs, Subcommand};
use reqwest::Method;
use serde_json::{json, Value};

use super::{obj, query, s, seg, Ctx};
use crate::http::Auth;

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    List,
    Get {
        webhook_id: String,
    },
    /// Event types that can be subscribed.
    EventTypes,
    /// Create a subscription. The signing secret is returned ONCE — use `--secret-out` to store it (0600).
    Create {
        #[arg(long)]
        url: String,
        /// Event types, e.g. `participant.joined` (repeatable or comma-separated); `*` for all.
        #[arg(long = "event", value_delimiter = ',', required = true)]
        events: Vec<String>,
        #[arg(long)]
        description: Option<String>,
        /// Write the secret to this file instead of printing it.
        #[arg(long, value_name = "PATH")]
        secret_out: Option<PathBuf>,
    },
    /// Update url/events/description/enabled.
    Update {
        webhook_id: String,
        #[arg(long)]
        url: Option<String>,
        #[arg(long = "event", value_delimiter = ',')]
        events: Vec<String>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        enabled: Option<bool>,
    },
    Delete {
        webhook_id: String,
    },
    /// Rotate the signing secret (returned once; `--secret-out` to store it).
    RotateSecret {
        webhook_id: String,
        #[arg(long, value_name = "PATH")]
        secret_out: Option<PathBuf>,
    },
    /// Send a synthetic `webhook.test` delivery.
    Test {
        webhook_id: String,
    },
    /// Re-enable after failures and replay the current state.
    Resync {
        webhook_id: String,
    },
    Deliveries {
        webhook_id: String,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        limit: Option<i64>,
        #[arg(long)]
        offset: Option<i64>,
    },
    Delivery {
        webhook_id: String,
        delivery_id: String,
    },
    Retry {
        webhook_id: String,
        delivery_id: String,
    },
    /// Verify an `X-Aurix-Signature` header against a raw body — offline, no network.
    Verify {
        /// File containing the webhook secret.
        #[arg(long, value_name = "PATH")]
        secret_file: PathBuf,
        /// The `X-Aurix-Signature` header value (`t=...,v1=...`).
        #[arg(long)]
        signature: String,
        /// Raw request body file (`-` for stdin).
        #[arg(long, value_name = "PATH")]
        body_file: PathBuf,
        #[arg(long, default_value_t = 300)]
        tolerance_secs: i64,
        /// Unix time to verify against (default: now).
        #[arg(long)]
        now: Option<i64>,
    },
    /// Sign a body with a secret (for building test fixtures).
    Sign {
        #[arg(long, value_name = "PATH")]
        secret_file: PathBuf,
        #[arg(long, value_name = "PATH")]
        body_file: PathBuf,
        #[arg(long)]
        timestamp: Option<i64>,
    },
}

fn read_body(path: &PathBuf) -> anyhow::Result<Vec<u8>> {
    if path.as_os_str() == "-" {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut std::io::stdin(), &mut buf)?;
        Ok(buf)
    } else {
        std::fs::read(path).map_err(|e| anyhow!("reading {}: {e}", path.display()))
    }
}

/// Redacts `secret` from a webhook response, optionally writing it to a private file.
fn handle_secret(ctx: &Ctx, mut v: Value, out: Option<PathBuf>) -> anyhow::Result<()> {
    if let Some(path) = out {
        let secret = v
            .get("secret")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("response has no `secret`"))?
            .to_string();
        crate::config::write_private(&path, format!("{secret}\n").as_bytes())?;
        if let Some(o) = v.as_object_mut() {
            o.remove("secret");
            o.insert("secret_file".into(), json!(path));
        }
    } else {
        eprintln!("note: the secret is shown once and never again; store it now (or use --secret-out PATH)");
    }
    ctx.print(&v)
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<()> {
    let api = &ctx.api;
    let a = Auth::ApiKey;
    match args.cmd {
        Cmd::List => ctx.print(&api.get("/v1/webhooks", &[], a).await?),
        Cmd::Get { webhook_id } => ctx.print(
            &api.get(&format!("/v1/webhooks/{}", seg(&webhook_id)), &[], a)
                .await?,
        ),
        Cmd::EventTypes => ctx.print(&api.get("/v1/webhooks/events", &[], a).await?),
        Cmd::Create {
            url,
            events,
            description,
            secret_out,
        } => {
            let body = obj(vec![
                ("url", Some(Value::String(url))),
                (
                    "events",
                    Some(Value::Array(
                        events.into_iter().map(Value::String).collect(),
                    )),
                ),
                ("description", s(&description)),
            ]);
            let v = api.post("/v1/webhooks", body, a).await?;
            handle_secret(ctx, v, secret_out)
        }
        Cmd::Update {
            webhook_id,
            url,
            events,
            description,
            enabled,
        } => {
            let body = obj(vec![
                ("url", s(&url)),
                (
                    "events",
                    (!events.is_empty())
                        .then(|| Value::Array(events.into_iter().map(Value::String).collect())),
                ),
                ("description", s(&description)),
                ("enabled", enabled.map(Value::Bool)),
            ]);
            if body.as_object().is_some_and(|o| o.is_empty()) {
                bail!("nothing to update");
            }
            ctx.print(
                &api.json(
                    Method::PATCH,
                    &format!("/v1/webhooks/{}", seg(&webhook_id)),
                    &[],
                    Some(&body),
                    a,
                )
                .await?,
            )
        }
        Cmd::Delete { webhook_id } => ctx.print(
            &api.delete(&format!("/v1/webhooks/{}", seg(&webhook_id)), &[], a)
                .await?,
        ),
        Cmd::RotateSecret {
            webhook_id,
            secret_out,
        } => {
            let v = api
                .post(
                    &format!("/v1/webhooks/{}/rotate-secret", seg(&webhook_id)),
                    json!({}),
                    a,
                )
                .await?;
            handle_secret(ctx, v, secret_out)
        }
        Cmd::Test { webhook_id } => ctx.print(
            &api.post(
                &format!("/v1/webhooks/{}/test", seg(&webhook_id)),
                json!({}),
                a,
            )
            .await?,
        ),
        Cmd::Resync { webhook_id } => ctx.print(
            &api.post(
                &format!("/v1/webhooks/{}/resync", seg(&webhook_id)),
                json!({}),
                a,
            )
            .await?,
        ),
        Cmd::Deliveries {
            webhook_id,
            status,
            limit,
            offset,
        } => {
            let q = query(vec![
                ("status", status),
                ("limit", limit.map(|v| v.to_string())),
                ("offset", offset.map(|v| v.to_string())),
            ]);
            ctx.print(
                &api.get(
                    &format!("/v1/webhooks/{}/deliveries", seg(&webhook_id)),
                    &q,
                    a,
                )
                .await?,
            )
        }
        Cmd::Delivery {
            webhook_id,
            delivery_id,
        } => ctx.print(
            &api.get(
                &format!(
                    "/v1/webhooks/{}/deliveries/{}",
                    seg(&webhook_id),
                    seg(&delivery_id)
                ),
                &[],
                a,
            )
            .await?,
        ),
        Cmd::Retry {
            webhook_id,
            delivery_id,
        } => ctx.print(
            &api.post(
                &format!(
                    "/v1/webhooks/{}/deliveries/{}/retry",
                    seg(&webhook_id),
                    seg(&delivery_id)
                ),
                json!({}),
                a,
            )
            .await?,
        ),
        Cmd::Verify {
            secret_file,
            signature,
            body_file,
            tolerance_secs,
            now,
        } => {
            let secret = crate::config::read_secret_file(&secret_file)?;
            let body = read_body(&body_file)?;
            let now = now.unwrap_or_else(|| chrono::Utc::now().timestamp());
            match crate::webhook_sig::verify(&secret, &signature, &body, now, tolerance_secs) {
                Ok(ts) => {
                    ctx.print(&json!({ "valid": true, "timestamp": ts, "skew_secs": now - ts }))
                }
                Err(reason) => {
                    ctx.print(&json!({ "valid": false, "reason": reason }))?;
                    std::process::exit(2);
                }
            }
        }
        Cmd::Sign {
            secret_file,
            body_file,
            timestamp,
        } => {
            let secret = crate::config::read_secret_file(&secret_file)?;
            let body = read_body(&body_file)?;
            let ts = timestamp.unwrap_or_else(|| chrono::Utc::now().timestamp());
            println!("{}", crate::webhook_sig::sign(&secret, ts, &body));
            Ok(())
        }
    }
}
