pub mod admin;
pub mod analytics;
pub mod api;
pub mod channel;
pub mod config;
pub mod diagnose;
pub mod moderate;
pub mod token;
pub mod user;
pub mod webhook;

use serde_json::{json, Value};

use crate::config::{self as cfg, Overrides};
use crate::http::{Api, Auth};
use crate::output::Printer;
use crate::{Cli, Command};

/// Everything a command needs: resolved connection, client, printer.
pub struct Ctx {
    pub resolved: cfg::Resolved,
    pub api: Api,
    pub out: Printer,
}

impl Ctx {
    pub fn print(&self, v: &Value) -> anyhow::Result<()> {
        self.out.print(v)
    }
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    let file = cfg::load(&cfg::config_path())?;
    let out = Printer {
        format: cli.output,
        field: cli.field.clone(),
    };

    // Config management must work without a resolvable profile.
    if let Command::Config(args) = &cli.command {
        return config::run(args, &file, &out);
    }

    let resolved = cfg::resolve(
        &file,
        &Overrides {
            profile: cli.profile.as_deref(),
            server: cli.server.as_deref(),
            api_key_file: cli.api_key_file.as_deref(),
            api_key_inline: cli.api_key.as_deref(),
            admin_token_file: cli.admin_token_file.as_deref(),
            timeout_secs: cli.timeout,
        },
        &cfg::system_env,
    )?;
    let bootstrap = match &cli.command {
        Command::Admin(a) => a.bootstrap_token()?,
        _ => None,
    };
    let api = Api::new(&resolved, bootstrap)?;
    let ctx = Ctx { resolved, api, out };

    match cli.command {
        Command::Health => ctx.print(&ctx.api.get("/health", &[], Auth::None).await?),
        Command::Ready => ctx.print(&ctx.api.get("/ready", &[], Auth::None).await?),
        Command::Version => {
            let node = match ctx.api.get("/health", &[], Auth::None).await {
                Ok(v) => v.get("version").cloned().unwrap_or(Value::Null),
                Err(e) => json!({ "error": e.to_string() }),
            };
            ctx.print(&json!({
                "cli": env!("CARGO_PKG_VERSION"),
                "contract": crate::openapi::spec().version,
                "node": node,
                "server": ctx.resolved.server,
            }))
        }
        Command::Config(_) => unreachable!("handled above"),
        Command::Token(a) => token::run(&ctx, a).await,
        Command::Turn(a) => token::turn(&ctx, a).await,
        Command::Regions(a) => token::regions(&ctx, a).await,
        Command::Channel(a) => channel::run(&ctx, a).await,
        Command::User(a) => user::run(&ctx, a).await,
        Command::Node(a) => user::nodes(&ctx, a).await,
        Command::Moderate(a) => moderate::run(&ctx, a).await,
        Command::Webhook(a) => webhook::run(&ctx, a).await,
        Command::Analytics(a) => analytics::run(&ctx, a).await,
        Command::Events(a) => analytics::events(&ctx, a).await,
        Command::Recording(a) => analytics::recordings(&ctx, a).await,
        Command::Admin(a) => admin::run(&ctx, a).await,
        Command::App(a) => admin::apps(&ctx, a).await,
        Command::Api(a) => api::run(&ctx, a).await,
        Command::Diagnose(a) => diagnose::run(&ctx, a).await,
    }
}

/// Builds a JSON object from `(key, Option<value>)` pairs, skipping `None`.
pub fn obj(fields: Vec<(&str, Option<Value>)>) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in fields {
        if let Some(v) = v {
            m.insert(k.to_string(), v);
        }
    }
    Value::Object(m)
}

pub fn s(v: &Option<String>) -> Option<Value> {
    v.as_ref().map(|x| Value::String(x.clone()))
}

pub fn i(v: &Option<i64>) -> Option<Value> {
    v.map(Value::from)
}

pub fn b(v: Option<bool>) -> Option<Value> {
    v.map(Value::Bool)
}

pub fn f(v: &Option<f64>) -> Option<Value> {
    v.map(Value::from)
}

/// Query-string pairs from optional values.
pub fn query(pairs: Vec<(&str, Option<String>)>) -> Vec<(String, String)> {
    pairs
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| (k.to_string(), v)))
        .collect()
}

pub fn seg(s: &str) -> String {
    crate::openapi::encode_segment(s)
}

/// Reads one secret line from stdin (password prompts) without echoing to stdout.
pub fn read_secret_stdin(what: &str) -> anyhow::Result<String> {
    eprint!("{what}: ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let v = line.trim_end_matches(['\r', '\n']).to_string();
    if v.is_empty() {
        anyhow::bail!("{what} is empty");
    }
    Ok(v)
}
