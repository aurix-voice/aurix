use anyhow::bail;
use clap::{Args as ClapArgs, Subcommand};
use serde_json::{json, Value};

use super::{obj, query, seg, Ctx};
use crate::http::Auth;

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Search users by display name / external id.
    Search {
        query: String,
        #[arg(long)]
        page: Option<i64>,
        #[arg(long)]
        per_page: Option<i64>,
    },
    Get {
        user_id: String,
    },
    /// Live stats of a session (MOS, RTT, loss, jitter).
    Session {
        session_id: String,
    },
    /// Delete a user and all their data (GDPR erasure).
    Delete {
        user_id: String,
        /// Also remove moderation history referencing the user.
        #[arg(long)]
        purge_moderation: bool,
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Export a user's data (GDPR access request) to stdout or a file.
    Export {
        user_id: String,
        #[arg(long, value_name = "PATH")]
        out: Option<std::path::PathBuf>,
    },
    /// Persistent blocks created by the user.
    Blocks {
        user_id: String,
    },
    Block {
        user_id: String,
        #[arg(long)]
        blocked: String,
    },
    Unblock {
        user_id: String,
        #[arg(long)]
        blocked: String,
    },
    /// Direct-message history between two users.
    Messages {
        user_id: String,
        #[arg(long)]
        peer: String,
        #[arg(long)]
        before: Option<String>,
        #[arg(long)]
        limit: Option<i64>,
    },
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<()> {
    let api = &ctx.api;
    match args.cmd {
        Cmd::Search {
            query: q,
            page,
            per_page,
        } => {
            let q = query(vec![
                ("q", Some(q)),
                ("page", page.map(|v| v.to_string())),
                ("per_page", per_page.map(|v| v.to_string())),
            ]);
            ctx.print(&api.get("/v1/users", &q, Auth::ApiKey).await?)
        }
        Cmd::Get { user_id } => ctx.print(
            &api.get(&format!("/v1/users/{}", seg(&user_id)), &[], Auth::ApiKey)
                .await?,
        ),
        Cmd::Session { session_id } => ctx.print(
            &api.get(
                &format!("/v1/sessions/{}/stats", seg(&session_id)),
                &[],
                Auth::ApiKey,
            )
            .await?,
        ),
        Cmd::Delete {
            user_id,
            purge_moderation,
            yes,
        } => {
            if !yes {
                bail!("this permanently erases the user and their data; re-run with --yes");
            }
            let q = query(vec![(
                "purge_moderation",
                purge_moderation.then(|| "true".to_string()),
            )]);
            ctx.print(
                &api.delete(&format!("/v1/users/{}", seg(&user_id)), &q, Auth::ApiKey)
                    .await?,
            )
        }
        Cmd::Export { user_id, out } => {
            let v = api
                .get(
                    &format!("/v1/users/{}/export", seg(&user_id)),
                    &[],
                    Auth::ApiKey,
                )
                .await?;
            match out {
                Some(path) => {
                    crate::config::write_private(
                        &path,
                        serde_json::to_string_pretty(&v)?.as_bytes(),
                    )?;
                    ctx.print(&json!({ "written": path, "bytes": serde_json::to_vec(&v)?.len() }))
                }
                None => ctx.print(&v),
            }
        }
        Cmd::Blocks { user_id } => ctx.print(
            &api.get(
                &format!("/v1/users/{}/blocks", seg(&user_id)),
                &[],
                Auth::ApiKey,
            )
            .await?,
        ),
        Cmd::Block { user_id, blocked } => ctx.print(
            &api.post(
                &format!("/v1/users/{}/blocks", seg(&user_id)),
                json!({ "blocked_user_id": blocked }),
                Auth::ApiKey,
            )
            .await?,
        ),
        Cmd::Unblock { user_id, blocked } => ctx.print(
            &api.delete(
                &format!("/v1/users/{}/blocks/{}", seg(&user_id), seg(&blocked)),
                &[],
                Auth::ApiKey,
            )
            .await?,
        ),
        Cmd::Messages {
            user_id,
            peer,
            before,
            limit,
        } => {
            let q = query(vec![
                ("peer", Some(peer)),
                ("before", before),
                ("limit", limit.map(|v| v.to_string())),
            ]);
            ctx.print(
                &api.get(
                    &format!("/v1/users/{}/messages", seg(&user_id)),
                    &q,
                    Auth::ApiKey,
                )
                .await?,
            )
        }
    }
}

#[derive(ClapArgs)]
pub struct NodeArgs {
    #[command(subcommand)]
    cmd: NodeCmd,
}

#[derive(Subcommand)]
enum NodeCmd {
    /// Media nodes with load and regions.
    List,
    /// Measured cascade links between nodes: transport (udp/tcp), RTT, age (admin).
    Links,
    /// Best endpoint for a player (`GET /v1/me/regions` semantics via API key).
    Endpoint {
        #[arg(long)]
        region: Option<String>,
    },
    /// Drain a node for maintenance: keeps its sessions, takes no new ones (admin).
    Drain {
        node_id: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// End a node drain (admin).
    Undrain { node_id: String },
    /// Effective configuration of the node behind the profile URL, secrets redacted (admin).
    Config,
}

pub async fn nodes(ctx: &Ctx, args: NodeArgs) -> anyhow::Result<()> {
    match args.cmd {
        NodeCmd::List => ctx.print(&ctx.api.get("/v1/nodes", &[], Auth::Admin).await?),
        NodeCmd::Links => ctx.print(&ctx.api.get("/v1/nodes/links", &[], Auth::Admin).await?),
        NodeCmd::Drain { node_id, reason } => ctx.print(
            &ctx.api
                .post(
                    &format!("/v1/nodes/{}/drain", seg(&node_id)),
                    json!({ "reason": reason }),
                    Auth::Admin,
                )
                .await?,
        ),
        NodeCmd::Undrain { node_id } => ctx.print(
            &ctx.api
                .post(
                    &format!("/v1/nodes/{}/undrain", seg(&node_id)),
                    json!({}),
                    Auth::Admin,
                )
                .await?,
        ),
        NodeCmd::Config => ctx.print(&ctx.api.get("/admin/config", &[], Auth::Admin).await?),
        NodeCmd::Endpoint { region } => {
            let q = query(vec![("region", region)]);
            let v = ctx.api.get("/v1/regions", &q, Auth::ApiKey).await?;
            let best = v
                .get("regions")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .cloned()
                .unwrap_or(Value::Null);
            ctx.print(&obj(vec![("best", Some(best)), ("all", Some(v))]))
        }
    }
}
