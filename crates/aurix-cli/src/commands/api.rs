use std::collections::BTreeMap;

use anyhow::bail;
use clap::Args as ClapArgs;
use reqwest::Method;
use serde_json::{json, Value};

use super::Ctx;
use crate::http::Auth;
use crate::openapi::{self, Operation};
use crate::output::{json_arg, kv};

#[derive(ClapArgs)]
pub struct Args {
    /// operationId (e.g. `listChannels`) or path template (`/v1/channels/{channel_id}`).
    #[arg(required_unless_present = "list")]
    operation: Option<String>,
    /// HTTP method when a path template has several.
    #[arg(long, short = 'X')]
    method: Option<String>,
    /// Path parameter `name=value` (repeatable).
    #[arg(long = "path", short = 'p', value_parser = kv)]
    path_params: Vec<(String, String)>,
    /// Query parameter `name=value` (repeatable).
    #[arg(long = "query", short = 'q', value_parser = kv)]
    query_params: Vec<(String, String)>,
    /// JSON request body: inline, `@file` or `-` (stdin).
    #[arg(long, short = 'd', value_name = "JSON|@file|-")]
    data: Option<String>,
    /// Credential to send (default: derived from the operation's security requirement).
    #[arg(long, value_parser = ["auto", "api-key", "admin", "none"], default_value = "auto")]
    auth: String,
    /// Print the raw body regardless of content type.
    #[arg(long)]
    raw: bool,
    /// Print the operation's parameters and schema instead of calling it.
    #[arg(long)]
    describe: bool,
    /// List every operation of the embedded contract (optionally filtered by substring).
    #[arg(long, value_name = "FILTER", num_args = 0..=1, default_missing_value = "")]
    list: Option<String>,
    /// Allow unknown query parameters.
    #[arg(long)]
    allow_unknown_query: bool,
}

fn pick_auth(op: &Operation, requested: &str) -> anyhow::Result<Auth> {
    Ok(match requested {
        "api-key" => Auth::ApiKey,
        "admin" => Auth::Admin,
        "none" => Auth::None,
        _ => {
            let s = &op.security;
            let api_key = s.iter().any(|x| x.starts_with("ApiKey"));
            let admin = s.iter().any(|x| x == "AdminToken");
            match (api_key, admin) {
                (true, true) => Auth::Any,
                (true, false) => Auth::ApiKey,
                (false, true) => Auth::Admin,
                (false, false) => {
                    if s.iter().any(|x| x == "PlayerToken") {
                        bail!(
                            "{} is a player endpoint (PlayerToken); the CLI has no player identity — call it from a game client or with --auth none against a dev node",
                            op.id
                        );
                    }
                    Auth::None
                }
            }
        }
    })
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<()> {
    let spec = openapi::spec();
    if let Some(filter) = &args.list {
        let f = filter.to_ascii_lowercase();
        let ops: Vec<Value> = spec
            .operations
            .values()
            .filter(|o| {
                f.is_empty()
                    || o.id.to_ascii_lowercase().contains(&f)
                    || o.path.to_ascii_lowercase().contains(&f)
                    || o.summary.to_ascii_lowercase().contains(&f)
            })
            .map(|o| {
                json!({
                    "id": o.id,
                    "method": o.method.as_str(),
                    "path": o.path,
                    "summary": o.summary,
                    "auth": o.security,
                    "deprecated": o.deprecated,
                })
            })
            .collect();
        return ctx.print(&json!({ "contract": spec.version, "operations": ops }));
    }

    let method_hint = args
        .method
        .as_deref()
        .map(|m| m.to_ascii_uppercase().parse::<Method>())
        .transpose()?;
    let selector = args
        .operation
        .as_deref()
        .expect("clap: required unless --list");
    let op = spec.find(selector, method_hint.as_ref())?;
    if let Some(m) = &method_hint {
        if *m != op.method {
            bail!("{} is {} {}, not {m}", op.id, op.method, op.path);
        }
    }

    if args.describe {
        let schema = op
            .request_schema
            .as_ref()
            .map(|n| spec.schemas.get(n).cloned().unwrap_or(Value::Null));
        return ctx.print(&json!({
            "id": op.id,
            "method": op.method.as_str(),
            "path": op.path,
            "summary": op.summary,
            "auth": op.security,
            "deprecated": op.deprecated,
            "path_params": op.path_params,
            "query_params": op.query_params.iter().map(|q| json!({ "name": q.name, "required": q.required, "description": q.description })).collect::<Vec<_>>(),
            "request_body": schema.map(|s| json!({ "schema": op.request_schema, "required": op.request_required, "definition": s })),
        }));
    }

    if op.deprecated {
        eprintln!("warning: {} is deprecated", op.id);
    }
    let path_params: BTreeMap<String, String> = args.path_params.into_iter().collect();
    let path = op.render_path(&path_params)?;

    if !args.allow_unknown_query {
        for (k, _) in &args.query_params {
            if !op.query_params.iter().any(|q| &q.name == k) {
                bail!(
                    "'{k}' is not a query parameter of {} (known: {}); pass --allow-unknown-query to send it anyway",
                    op.id,
                    op.query_params.iter().map(|q| q.name.as_str()).collect::<Vec<_>>().join(", ")
                );
            }
        }
    }
    for q in op.query_params.iter().filter(|q| q.required) {
        if !args.query_params.iter().any(|(k, _)| k == &q.name) {
            bail!(
                "missing required query parameter '{}' (pass -q {}=VALUE)",
                q.name,
                q.name
            );
        }
    }

    let body = args.data.as_deref().map(json_arg).transpose()?;
    if body.is_some()
        && op.request_schema.is_none()
        && matches!(op.method, Method::GET | Method::DELETE)
    {
        bail!("{} takes no request body", op.id);
    }
    if body.is_none() && op.request_required {
        bail!(
            "{} requires a JSON body{} (pass -d '{{...}}' or -d @file; see --describe)",
            op.id,
            op.request_schema
                .as_ref()
                .map(|s| format!(" ({s})"))
                .unwrap_or_default()
        );
    }

    let auth = pick_auth(op, &args.auth)?;
    let resp = ctx
        .api
        .send(
            op.method.clone(),
            &path,
            &args.query_params,
            body.as_ref(),
            auth,
        )
        .await?;
    let is_json =
        resp.content_type.starts_with("application/json") || resp.content_type.contains("+json");
    if args.raw || !is_json {
        use std::io::Write;
        std::io::stdout().write_all(&resp.body)?;
        if !resp.body.ends_with(b"\n") {
            println!();
        }
        return Ok(());
    }
    if resp.body.is_empty() {
        return ctx.print(&json!({ "status": resp.status.as_u16() }));
    }
    let v: Value = serde_json::from_slice(&resp.body)?;
    ctx.print(&v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_derivation() {
        let s = openapi::spec();
        assert_eq!(
            pick_auth(s.find("issueToken", None).unwrap(), "auto").unwrap(),
            Auth::ApiKey
        );
        assert_eq!(
            pick_auth(s.find("listApps", None).unwrap(), "auto").unwrap(),
            Auth::Admin
        );
        assert_eq!(
            pick_auth(s.find("health", None).unwrap(), "auto").unwrap(),
            Auth::None
        );
        assert!(pick_auth(s.find("myRegions", None).unwrap(), "auto").is_err());
        assert_eq!(
            pick_auth(s.find("myRegions", None).unwrap(), "none").unwrap(),
            Auth::None
        );
    }
}
