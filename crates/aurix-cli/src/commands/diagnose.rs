use std::time::Instant;

use clap::Args as ClapArgs;
use serde_json::{json, Value};

use super::Ctx;
use crate::http::{ApiError, Auth};

#[derive(ClapArgs)]
pub struct Args {
    /// Skip the credential probes (only connectivity and contract).
    #[arg(long)]
    no_auth: bool,
}

fn outcome(res: &anyhow::Result<Value>, ms: u128) -> (bool, Value) {
    match res {
        Ok(_) => (true, json!({ "ok": true, "ms": ms })),
        Err(e) => {
            if let Some(api) = e.downcast_ref::<ApiError>() {
                (
                    false,
                    json!({ "ok": false, "ms": ms, "status": api.status.as_u16(), "code": api.code, "message": api.message }),
                )
            } else {
                (
                    false,
                    json!({ "ok": false, "ms": ms, "error": format!("{e:#}") }),
                )
            }
        }
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<()> {
    let api = &ctx.api;
    let r = &ctx.resolved;
    let mut checks = serde_json::Map::new();
    let mut all_ok = true;
    let mut hints: Vec<String> = Vec::new();

    let t = Instant::now();
    let health = api.get("/health", &[], Auth::None).await;
    let (ok, mut v) = outcome(&health, t.elapsed().as_millis());
    if let Ok(h) = &health {
        v["version"] = h.get("version").cloned().unwrap_or(Value::Null);
        v["node_id"] = h.get("node_id").cloned().unwrap_or(Value::Null);
    }
    if !ok {
        hints.push(format!(
            "cannot reach {} — check --server / AURIX_SERVER / profile",
            r.server
        ));
    }
    all_ok &= ok;
    checks.insert("health".into(), v);

    let t = Instant::now();
    let ready = api.get("/ready", &[], Auth::None).await;
    let (ok, mut v) = outcome(&ready, t.elapsed().as_millis());
    if let Ok(rd) = &ready {
        for k in ["database", "redis", "checks", "status"] {
            if let Some(x) = rd.get(k) {
                v[k] = x.clone();
            }
        }
    }
    if !ok && health.is_ok() {
        hints.push("node is up but not ready: database or Redis is unavailable".into());
    }
    all_ok &= ok;
    checks.insert("ready".into(), v);

    let t = Instant::now();
    let remote = api.get("/openapi.json", &[], Auth::None).await;
    let (ok, mut v) = outcome(&remote, t.elapsed().as_millis());
    let local = crate::openapi::spec();
    v["cli_contract"] = json!(local.version);
    if let Ok(doc) = &remote {
        let rv = doc["info"]["version"].as_str().unwrap_or("unknown");
        v["node_contract"] = json!(rv);
        let remote_ops: std::collections::BTreeSet<String> = doc["paths"]
            .as_object()
            .into_iter()
            .flat_map(|paths| paths.values())
            .flat_map(|item| item.as_object().into_iter().flat_map(|m| m.values()))
            .filter_map(|op| {
                op.get("operationId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        let missing: Vec<&String> = local
            .operations
            .keys()
            .filter(|k| !remote_ops.contains(*k))
            .collect();
        let extra: Vec<&String> = remote_ops
            .iter()
            .filter(|k| !local.operations.contains_key(*k))
            .collect();
        v["match"] = json!(rv == local.version && missing.is_empty() && extra.is_empty());
        v["operations_missing_on_node"] = json!(missing);
        v["operations_unknown_to_cli"] = json!(extra);
        if rv != local.version {
            hints.push(format!("contract mismatch: CLI speaks {} but the node serves {rv} — upgrade the older side", local.version));
        }
    }
    all_ok &= ok;
    checks.insert("contract".into(), v);

    if !args.no_auth {
        let mut auth = serde_json::Map::new();
        auth.insert("api_key_source".into(), json!(r.api_key_source));
        auth.insert("admin_token_source".into(), json!(r.admin_token_source));
        if api.has_api_key() {
            let t = Instant::now();
            let res = api
                .get(
                    "/v1/channels",
                    &[("per_page".into(), "1".into())],
                    Auth::ApiKey,
                )
                .await;
            let (ok, v) = outcome(&res, t.elapsed().as_millis());
            if !ok {
                hints.push("API key rejected: rotate it (`aurix app rotate-key`) or fix the profile's api_key_file".into());
                all_ok = false;
            }
            auth.insert("api_key".into(), v);
        } else {
            auth.insert(
                "api_key".into(),
                json!({ "ok": null, "skipped": "no API key configured" }),
            );
        }
        if api.has_admin_token() {
            let t = Instant::now();
            let res = api.get("/admin/me", &[], Auth::Admin).await;
            let (ok, mut v) = outcome(&res, t.elapsed().as_millis());
            if let Ok(me) = &res {
                v["email"] = me.get("email").cloned().unwrap_or(Value::Null);
                v["role"] = me.get("role").cloned().unwrap_or(Value::Null);
            }
            if !ok {
                hints.push("operator token rejected or expired: `aurix admin login --save`".into());
                all_ok = false;
            }
            auth.insert("admin_token".into(), v);
        } else {
            auth.insert(
                "admin_token".into(),
                json!({ "ok": null, "skipped": "no operator token configured" }),
            );
        }
        checks.insert("auth".into(), Value::Object(auth));
    }

    if let Ok(h) = &health {
        if let Some(nodes) = h.get("nodes").or_else(|| h.get("fleet")) {
            checks.insert("fleet".into(), nodes.clone());
        }
    }

    if let Some(date) = api
        .send(reqwest::Method::GET, "/health", &[], None, Auth::None)
        .await
        .ok()
        .and_then(|r| r.date)
    {
        if let Ok(server_time) = chrono::DateTime::parse_from_rfc2822(&date) {
            let skew = (chrono::Utc::now() - server_time.with_timezone(&chrono::Utc)).num_seconds();
            checks.insert("clock_skew_secs".into(), json!(skew));
            if skew.abs() > 30 {
                hints.push(format!("clock skew of {skew}s: tokens and webhook signatures have a 5-minute tolerance"));
            }
        }
    }

    ctx.print(&json!({
        "ok": all_ok,
        "profile": r.profile_name,
        "server": r.server,
        "cli": env!("CARGO_PKG_VERSION"),
        "checks": checks,
        "hints": hints,
    }))?;
    if !all_ok {
        std::process::exit(2);
    }
    Ok(())
}
