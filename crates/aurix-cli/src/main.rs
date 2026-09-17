use clap::{Parser, Subcommand};
use serde_json::Value;

#[derive(Parser)]
#[command(name = "aurix", version, about = "Aurix Voice Platform CLI")]
struct Cli {
    #[arg(long, default_value = "http://localhost:8080", global = true)]
    server: String,

    #[arg(long, global = true)]
    api_key: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Check server health
    Health,

    /// Generate a user token
    Token {
        #[arg(long)]
        user_id: String,
        #[arg(long)]
        app_id: String,
        #[arg(long)]
        display_name: String,
    },

    /// Channel management
    Channel {
        #[command(subcommand)]
        action: ChannelAction,
    },

    /// User management
    User {
        #[command(subcommand)]
        action: UserAction,
    },

    /// Node management
    Node {
        #[command(subcommand)]
        action: NodeAction,
    },

    /// Moderation
    Moderate {
        #[command(subcommand)]
        action: ModerateAction,
    },

    /// Get TURN credentials
    TurnCredentials {
        #[arg(long)]
        user_id: String,
    },

    /// Run connectivity diagnostics
    Diagnose {
        #[arg(long)]
        target: Option<String>,
    },
}

#[derive(Subcommand)]
enum ChannelAction {
    List,
    Create {
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "team")]
        channel_type: String,
    },
    Get {
        #[arg(long)]
        id: String,
    },
    Delete {
        #[arg(long)]
        id: String,
    },
    Participants {
        #[arg(long)]
        id: String,
    },
}

#[derive(Subcommand)]
enum UserAction {
    Search {
        #[arg(long)]
        query: String,
    },
    Get {
        #[arg(long)]
        id: String,
    },
}

#[derive(Subcommand)]
enum NodeAction {
    List,
}

#[derive(Subcommand)]
enum ModerateAction {
    Ban {
        #[arg(long)]
        user_id: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        duration_hours: Option<i64>,
    },
    Mute {
        #[arg(long)]
        user_id: String,
        #[arg(long)]
        channel_id: String,
    },
    Kick {
        #[arg(long)]
        user_id: String,
        #[arg(long)]
        channel_id: String,
        #[arg(long)]
        reason: String,
    },
    Events {
        #[arg(long)]
        status: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let client = reqwest::Client::new();
    let base_url = &cli.server;

    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(ref key) = cli.api_key {
        headers.insert("X-API-Key", key.parse()?);
    }

    match cli.command {
        Commands::Health => {
            let resp = client
                .get(format!("{}/health", base_url))
                .send()
                .await?
                .json::<Value>()
                .await?;
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }

        Commands::Token {
            user_id,
            app_id,
            display_name,
        } => {
            let body = serde_json::json!({
                "user_id": user_id,
                "app_id": app_id,
                "display_name": display_name,
                "channels": [],
            });
            let resp = client
                .post(format!("{}/v1/tokens", base_url))
                .headers(headers)
                .json(&body)
                .send()
                .await?
                .json::<Value>()
                .await?;
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }

        Commands::Channel { action } => match action {
            ChannelAction::List => {
                let resp = client
                    .get(format!("{}/v1/channels", base_url))
                    .headers(headers)
                    .send()
                    .await?
                    .json::<Value>()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
            ChannelAction::Create { name, channel_type } => {
                let body = serde_json::json!({
                    "name": name,
                    "config": {
                        "channel_type": channel_type,
                    }
                });
                let resp = client
                    .post(format!("{}/v1/channels", base_url))
                    .headers(headers)
                    .json(&body)
                    .send()
                    .await?
                    .json::<Value>()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
            ChannelAction::Get { id } => {
                let resp = client
                    .get(format!("{}/v1/channels/{}", base_url, id))
                    .headers(headers)
                    .send()
                    .await?
                    .json::<Value>()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
            ChannelAction::Delete { id } => {
                let resp = client
                    .delete(format!("{}/v1/channels/{}", base_url, id))
                    .headers(headers)
                    .send()
                    .await?
                    .json::<Value>()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
            ChannelAction::Participants { id } => {
                let resp = client
                    .get(format!("{}/v1/channels/{}/participants", base_url, id))
                    .headers(headers)
                    .send()
                    .await?
                    .json::<Value>()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
        },

        Commands::User { action } => match action {
            UserAction::Search { query } => {
                let resp = client
                    .get(format!("{}/v1/users?q={}", base_url, query))
                    .headers(headers)
                    .send()
                    .await?
                    .json::<Value>()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
            UserAction::Get { id } => {
                let resp = client
                    .get(format!("{}/v1/users/{}", base_url, id))
                    .headers(headers)
                    .send()
                    .await?
                    .json::<Value>()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
        },

        Commands::Node { action } => match action {
            NodeAction::List => {
                let resp = client
                    .get(format!("{}/v1/nodes", base_url))
                    .headers(headers)
                    .send()
                    .await?
                    .json::<Value>()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
        },

        Commands::Moderate { action } => match action {
            ModerateAction::Ban {
                user_id,
                reason,
                duration_hours,
            } => {
                let body = serde_json::json!({
                    "user_id": user_id,
                    "scope": "account",
                    "reason": reason,
                    "duration_hours": duration_hours,
                });
                let resp = client
                    .post(format!("{}/v1/moderation/ban", base_url))
                    .headers(headers)
                    .json(&body)
                    .send()
                    .await?
                    .json::<Value>()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
            ModerateAction::Mute {
                user_id,
                channel_id,
            } => {
                let body = serde_json::json!({
                    "user_id": user_id,
                    "channel_id": channel_id,
                    "muted": true,
                });
                let resp = client
                    .post(format!("{}/v1/moderation/mute", base_url))
                    .headers(headers)
                    .json(&body)
                    .send()
                    .await?
                    .json::<Value>()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
            ModerateAction::Kick {
                user_id,
                channel_id,
                reason,
            } => {
                let body = serde_json::json!({
                    "user_id": user_id,
                    "channel_id": channel_id,
                    "reason": reason,
                });
                let resp = client
                    .post(format!("{}/v1/moderation/kick", base_url))
                    .headers(headers)
                    .json(&body)
                    .send()
                    .await?
                    .json::<Value>()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
            ModerateAction::Events { status } => {
                let url = match status {
                    Some(s) => format!("{}/v1/moderation/events?status={}", base_url, s),
                    None => format!("{}/v1/moderation/events", base_url),
                };
                let resp = client
                    .get(url)
                    .headers(headers)
                    .send()
                    .await?
                    .json::<Value>()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
        },

        Commands::TurnCredentials { user_id } => {
            let body = serde_json::json!({ "user_id": user_id });
            let resp = client
                .post(format!("{}/v1/turn/credentials", base_url))
                .headers(headers)
                .json(&body)
                .send()
                .await?
                .json::<Value>()
                .await?;
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }

        Commands::Diagnose { target } => {
            println!("Running connectivity diagnostics...");
            let target_url = target.unwrap_or_else(|| base_url.clone());

            // HTTP connectivity
            print!("  HTTP connectivity... ");
            match client.get(format!("{}/health", target_url)).send().await {
                Ok(resp) => println!("OK (status: {})", resp.status()),
                Err(e) => println!("FAILED: {}", e),
            }

            // WebSocket connectivity
            print!("  WebSocket endpoint... ");
            match client
                .get(format!(
                    "{}/ws",
                    target_url.replace("http", "ws")
                ))
                .send()
                .await
            {
                Ok(_) => println!("Reachable"),
                Err(e) => println!("FAILED: {}", e),
            }

            // Metrics endpoint
            print!("  Metrics endpoint... ");
            match client
                .get(format!("{}/metrics", target_url))
                .send()
                .await
            {
                Ok(resp) => println!("OK ({} bytes)", resp.content_length().unwrap_or(0)),
                Err(e) => println!("FAILED: {}", e),
            }

            println!("Diagnostics complete.");
        }
    }

    Ok(())
}