//! `aurix` — operator / game-backend CLI for the Aurix REST API.
//!
//! Credentials are read from files, environment variables or profiles and are never
//! printed. Every command speaks the contract in `api/openapi.json` (embedded); anything
//! without a curated command is reachable through `aurix api <operationId>`.

mod commands;
mod config;
mod http;
mod openapi;
mod output;
mod webhook_sig;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::http::{ApiError, NetworkError};
use crate::output::Format;

#[derive(Parser)]
#[command(name = "aurix", version, about = "Aurix game-voice platform CLI", long_about = None)]
pub struct Cli {
    /// Profile from the config file (`AURIX_PROFILE`).
    #[arg(long, global = true, env = "AURIX_PROFILE")]
    profile: Option<String>,

    /// Node HTTP origin, e.g. https://voice.example.com (`AURIX_SERVER`).
    #[arg(long, global = true)]
    server: Option<String>,

    /// File containing the application API key (mode 0600).
    #[arg(long, global = true, value_name = "PATH")]
    api_key_file: Option<PathBuf>,

    /// API key on the command line — visible in process listings; prefer --api-key-file or AURIX_API_KEY.
    #[arg(long, global = true, hide = true)]
    api_key: Option<String>,

    /// File containing an operator (admin) JWT.
    #[arg(long, global = true, value_name = "PATH")]
    admin_token_file: Option<PathBuf>,

    /// Per-request timeout in seconds.
    #[arg(long, global = true, value_name = "SECS")]
    timeout: Option<u64>,

    /// Output format.
    #[arg(short = 'o', long, global = true, value_enum, default_value_t = Format::Pretty)]
    output: Format,

    /// Print only this field of the response (dotted path, e.g. `token` or `endpoint.ws_url`).
    #[arg(long, global = true, value_name = "PATH")]
    field: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Node health (no credentials).
    Health,
    /// Readiness incl. database/Redis (no credentials).
    Ready,
    /// CLI, embedded contract and node versions.
    Version,
    /// Profiles and credential locations.
    Config(commands::config::Args),
    /// Player tokens (backend-only: needs the API key).
    Token(commands::token::Args),
    /// TURN credentials for a player.
    Turn(commands::token::TurnArgs),
    /// Region discovery.
    Regions(commands::token::RegionsArgs),
    /// Channels.
    Channel(commands::channel::Args),
    /// Users, exports, blocks.
    User(commands::user::Args),
    /// Media nodes of the fleet.
    Node(commands::user::NodeArgs),
    /// Bans, mutes, kicks, priority, moderation events.
    Moderate(commands::moderate::Args),
    /// Webhook subscriptions, deliveries and local signature verification.
    Webhook(commands::webhook::Args),
    /// Usage, quota and quality analytics.
    Analytics(commands::analytics::Args),
    /// Live event stream (SSE) and snapshot.
    Events(commands::analytics::EventsArgs),
    /// Recordings.
    Recording(commands::analytics::RecordingArgs),
    /// Operator (admin) endpoints: setup, login, admins, audit log, fleet usage.
    Admin(commands::admin::Args),
    /// Apps and API keys (operator).
    App(commands::admin::AppArgs),
    /// Call any operation of the embedded OpenAPI contract.
    Api(commands::api::Args),
    /// Connectivity, auth and contract diagnostics.
    Diagnose(commands::diagnose::Args),
    /// Local node preflight: configuration, ports, certificates, Redis, PostgreSQL, migrations.
    Doctor(commands::doctor::Args),
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let json_errors = matches!(cli.output, Format::Json);
    let code = match commands::run(cli).await {
        Ok(()) => 0,
        Err(err) => {
            if let Some(api) = err.downcast_ref::<ApiError>() {
                if json_errors {
                    eprintln!("{}", api.to_json());
                } else {
                    eprintln!("error: {api}");
                    if let Some(ra) = &api.retry_after {
                        eprintln!("retry after {ra}s");
                    }
                    if api.status.as_u16() == 401 {
                        eprintln!("hint: check which credential the profile resolves (`aurix config show`)");
                    }
                }
                api.exit_code()
            } else if err
                .chain()
                .any(|c| c.downcast_ref::<NetworkError>().is_some())
            {
                eprintln!("error: {err:#}");
                6
            } else {
                eprintln!("error: {err:#}");
                1
            }
        }
    };
    std::process::exit(code);
}
