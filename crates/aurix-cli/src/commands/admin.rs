use std::path::PathBuf;

use anyhow::bail;
use clap::{Args as ClapArgs, Subcommand};
use reqwest::Method;
use serde_json::{json, Value};

use super::{i, obj, query, read_secret_stdin, s, seg, Ctx};
use crate::config;
use crate::http::Auth;
use crate::output::json_arg;

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

impl Args {
    /// The bootstrap token is only ever read from a file and only for `admin setup`.
    pub fn bootstrap_token(&self) -> anyhow::Result<Option<String>> {
        match &self.cmd {
            Cmd::Setup {
                bootstrap_token_file,
                ..
            } => Ok(Some(config::read_secret_file(bootstrap_token_file)?)),
            _ => Ok(None),
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the first operator account (password read from stdin).
    Setup {
        #[arg(long)]
        email: String,
        #[arg(long)]
        display_name: String,
        /// File holding `auth.bootstrap_token` of the node.
        #[arg(long, value_name = "PATH")]
        bootstrap_token_file: PathBuf,
    },
    /// Log in with email + password (from stdin); `--save` stores the JWT for the profile (0600).
    Login {
        #[arg(long)]
        email: String,
        #[arg(long)]
        save: bool,
    },
    /// Available operator login methods (password / OIDC).
    AuthMethods,
    /// The current operator.
    Me,
    /// Change the current operator's password (both read from stdin).
    ChangePassword,
    /// Revoke every token of the current operator.
    LogoutAll,
    Admins,
    AdminGet {
        admin_id: String,
    },
    AdminCreate {
        #[arg(long)]
        email: String,
        #[arg(long)]
        display_name: String,
        #[arg(long)]
        role: Option<String>,
    },
    AdminUpdate {
        admin_id: String,
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        display_name: Option<String>,
        #[arg(long)]
        active: Option<bool>,
    },
    /// Set a new password for another operator (read from stdin).
    AdminResetPassword {
        admin_id: String,
    },
    AdminLogoutAll {
        admin_id: String,
    },
    /// Operator audit log.
    AuditLog {
        #[arg(long)]
        page: Option<i64>,
        #[arg(long)]
        per_page: Option<i64>,
    },
    /// Run a retention sweep now.
    RetentionSweep,
    /// Fleet-wide usage across apps.
    Usage {
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
    },
    AppUsage {
        app_id: String,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        step: Option<String>,
    },
    UsageExport {
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long, default_value = "json", value_parser = ["json", "csv"])]
        format: String,
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<()> {
    let api = &ctx.api;
    let a = Auth::Admin;
    match args.cmd {
        Cmd::Setup {
            email,
            display_name,
            ..
        } => {
            let password = read_secret_stdin("password")?;
            let v = api
                .post(
                    "/admin/setup",
                    json!({ "email": email, "password": password, "display_name": display_name }),
                    Auth::None,
                )
                .await?;
            ctx.print(&redact_token(v))
        }
        Cmd::Login { email, save } => {
            let password = read_secret_stdin("password")?;
            let v = api
                .post(
                    "/admin/login",
                    json!({ "email": email, "password": password }),
                    Auth::None,
                )
                .await?;
            if save {
                let token = v
                    .get("token")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("login response has no `token`"))?;
                let path = config::admin_token_path(&ctx.resolved.profile_name);
                config::write_private(&path, format!("{token}\n").as_bytes())?;
                let mut redacted = redact_token(v);
                if let Some(o) = redacted.as_object_mut() {
                    o.insert("token_file".into(), json!(path));
                }
                eprintln!(
                    "saved operator token for profile '{}' → {}",
                    ctx.resolved.profile_name,
                    path.display()
                );
                return ctx.print(&redacted);
            }
            eprintln!(
                "note: printing the operator token; use --save to store it for the profile instead"
            );
            ctx.print(&v)
        }
        Cmd::AuthMethods => ctx.print(&api.get("/admin/auth/methods", &[], Auth::None).await?),
        Cmd::Me => ctx.print(&api.get("/admin/me", &[], a).await?),
        Cmd::ChangePassword => {
            let current = read_secret_stdin("current password")?;
            let new = read_secret_stdin("new password")?;
            ctx.print(
                &api.post(
                    "/admin/me/password",
                    json!({ "current_password": current, "new_password": new }),
                    a,
                )
                .await?,
            )
        }
        Cmd::LogoutAll => ctx.print(&api.post("/admin/logout-all", json!({}), a).await?),
        Cmd::Admins => ctx.print(&api.get("/admin/admins", &[], a).await?),
        Cmd::AdminGet { admin_id } => ctx.print(
            &api.get(&format!("/admin/admins/{}", seg(&admin_id)), &[], a)
                .await?,
        ),
        Cmd::AdminCreate {
            email,
            display_name,
            role,
        } => {
            let password = read_secret_stdin("initial password")?;
            let body = obj(vec![
                ("email", Some(Value::String(email))),
                ("password", Some(Value::String(password))),
                ("display_name", Some(Value::String(display_name))),
                ("role", s(&role)),
            ]);
            ctx.print(&api.post("/admin/admins", body, a).await?)
        }
        Cmd::AdminUpdate {
            admin_id,
            role,
            display_name,
            active,
        } => {
            let body = obj(vec![
                ("role", s(&role)),
                ("display_name", s(&display_name)),
                ("active", active.map(Value::Bool)),
            ]);
            if body.as_object().is_some_and(|o| o.is_empty()) {
                bail!("nothing to update");
            }
            ctx.print(
                &api.json(
                    Method::PATCH,
                    &format!("/admin/admins/{}", seg(&admin_id)),
                    &[],
                    Some(&body),
                    a,
                )
                .await?,
            )
        }
        Cmd::AdminResetPassword { admin_id } => {
            let password = read_secret_stdin("new password")?;
            ctx.print(
                &api.post(
                    &format!("/admin/admins/{}/password", seg(&admin_id)),
                    json!({ "password": password }),
                    a,
                )
                .await?,
            )
        }
        Cmd::AdminLogoutAll { admin_id } => ctx.print(
            &api.post(
                &format!("/admin/admins/{}/logout-all", seg(&admin_id)),
                json!({}),
                a,
            )
            .await?,
        ),
        Cmd::AuditLog { page, per_page } => {
            let q = query(vec![
                ("page", page.map(|v| v.to_string())),
                ("per_page", per_page.map(|v| v.to_string())),
            ]);
            ctx.print(&api.get("/admin/audit-log", &q, a).await?)
        }
        Cmd::RetentionSweep => ctx.print(&api.post("/admin/retention/sweep", json!({}), a).await?),
        Cmd::Usage { from, to } => {
            let q = query(vec![("from", from), ("to", to)]);
            ctx.print(&api.get("/admin/analytics/usage", &q, a).await?)
        }
        Cmd::AppUsage {
            app_id,
            from,
            to,
            step,
        } => {
            let q = query(vec![("from", from), ("to", to), ("step", step)]);
            ctx.print(
                &api.get(&format!("/admin/analytics/apps/{}", seg(&app_id)), &q, a)
                    .await?,
            )
        }
        Cmd::UsageExport {
            from,
            to,
            format,
            out,
        } => {
            let q = query(vec![
                ("from", from),
                ("to", to),
                ("format", Some(format.clone())),
            ]);
            let resp = api
                .send(Method::GET, "/admin/analytics/export", &q, None, a)
                .await?;
            super::analytics::write_export(
                ctx,
                resp.body,
                &resp.content_type,
                format == "json",
                out,
            )
        }
    }
}

fn redact_token(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        if o.remove("token").is_some() {
            o.insert("token".into(), Value::String("<redacted>".into()));
        }
    }
    v
}

#[derive(ClapArgs)]
pub struct AppArgs {
    #[command(subcommand)]
    cmd: AppCmd,
}

#[derive(Subcommand)]
enum AppCmd {
    /// List apps (operator).
    List {
        #[arg(long)]
        page: Option<i64>,
        #[arg(long)]
        per_page: Option<i64>,
    },
    Get {
        app_id: String,
    },
    /// Create an app; the API key is returned once — `--key-out` stores it (0600).
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        max_channels: Option<i64>,
        #[arg(long)]
        max_participants_per_channel: Option<i64>,
        #[arg(long)]
        max_concurrent_sessions: Option<i64>,
        #[arg(long)]
        monthly_participant_minutes: Option<i64>,
        #[arg(long, value_name = "PATH")]
        key_out: Option<PathBuf>,
    },
    /// Patch limits/name (`--data` is an UpdateAppRequest JSON).
    Update {
        app_id: String,
        #[arg(long, value_name = "JSON|@file|-")]
        data: String,
    },
    Delete {
        app_id: String,
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Rotate the app's primary API key (returned once; `--key-out` stores it).
    RotateKey {
        app_id: String,
        #[arg(long, value_name = "PATH")]
        key_out: Option<PathBuf>,
    },
    /// Additional API keys of the *current* app (API-key auth).
    Keys,
    /// Create an additional API key for the current app (returned once; `--key-out`).
    KeyCreate {
        #[arg(long)]
        name: String,
        #[arg(long = "permission")]
        permissions: Vec<String>,
        #[arg(long)]
        rate_limit: Option<i64>,
        #[arg(long)]
        expires_in_days: Option<i64>,
        #[arg(long, value_name = "PATH")]
        key_out: Option<PathBuf>,
    },
    KeyUpdate {
        key_id: String,
        #[arg(long)]
        rate_limit: i64,
    },
    KeyRevoke {
        key_id: String,
    },
    /// App-scoped audit log (API-key auth).
    AuditLog {
        #[arg(long)]
        page: Option<i64>,
        #[arg(long)]
        per_page: Option<i64>,
    },
}

/// Moves any `api_key`/`key` field of the response into a private file (or warns).
fn handle_key(ctx: &Ctx, mut v: Value, out: Option<PathBuf>) -> anyhow::Result<()> {
    let field = ["api_key", "key", "secret"]
        .into_iter()
        .find(|f| v.get(f).and_then(Value::as_str).is_some());
    match (field, out) {
        (Some(f), Some(path)) => {
            let key = v[f].as_str().unwrap_or_default().to_string();
            config::write_private(&path, format!("{key}\n").as_bytes())?;
            if let Some(o) = v.as_object_mut() {
                o.remove(f);
                o.insert(format!("{f}_file"), json!(path));
            }
        }
        (Some(f), None) => eprintln!(
            "note: `{f}` is shown once and never again; store it now (or use --key-out PATH)"
        ),
        (None, Some(_)) => bail!("response contains no key to save"),
        (None, None) => {}
    }
    ctx.print(&v)
}

pub async fn apps(ctx: &Ctx, args: AppArgs) -> anyhow::Result<()> {
    let api = &ctx.api;
    match args.cmd {
        AppCmd::List { page, per_page } => {
            let q = query(vec![
                ("page", page.map(|v| v.to_string())),
                ("per_page", per_page.map(|v| v.to_string())),
            ]);
            ctx.print(&api.get("/v1/apps", &q, Auth::Admin).await?)
        }
        AppCmd::Get { app_id } => ctx.print(
            &api.get(&format!("/v1/apps/{}", seg(&app_id)), &[], Auth::Admin)
                .await?,
        ),
        AppCmd::Create {
            name,
            description,
            max_channels,
            max_participants_per_channel,
            max_concurrent_sessions,
            monthly_participant_minutes,
            key_out,
        } => {
            let body = obj(vec![
                ("name", Some(Value::String(name))),
                ("description", s(&description)),
                ("max_channels", i(&max_channels)),
                (
                    "max_participants_per_channel",
                    i(&max_participants_per_channel),
                ),
                ("max_concurrent_sessions", i(&max_concurrent_sessions)),
                (
                    "monthly_participant_minutes",
                    i(&monthly_participant_minutes),
                ),
            ]);
            let v = api.post("/v1/apps", body, Auth::Admin).await?;
            handle_key(ctx, v, key_out)
        }
        AppCmd::Update { app_id, data } => {
            let body = json_arg(&data)?;
            ctx.print(
                &api.json(
                    Method::PATCH,
                    &format!("/v1/apps/{}", seg(&app_id)),
                    &[],
                    Some(&body),
                    Auth::Admin,
                )
                .await?,
            )
        }
        AppCmd::Delete { app_id, yes } => {
            if !yes {
                bail!("deleting an app removes its channels, users and keys; re-run with --yes");
            }
            ctx.print(
                &api.delete(&format!("/v1/apps/{}", seg(&app_id)), &[], Auth::Admin)
                    .await?,
            )
        }
        AppCmd::RotateKey { app_id, key_out } => {
            let v = api
                .post(
                    &format!("/v1/apps/{}/rotate-key", seg(&app_id)),
                    json!({}),
                    Auth::Admin,
                )
                .await?;
            handle_key(ctx, v, key_out)
        }
        AppCmd::Keys => ctx.print(&api.get("/v1/api-keys", &[], Auth::ApiKey).await?),
        AppCmd::KeyCreate {
            name,
            permissions,
            rate_limit,
            expires_in_days,
            key_out,
        } => {
            let body = obj(vec![
                ("name", Some(Value::String(name))),
                (
                    "permissions",
                    (!permissions.is_empty()).then(|| {
                        Value::Array(permissions.into_iter().map(Value::String).collect())
                    }),
                ),
                ("rate_limit", i(&rate_limit)),
                ("expires_in_days", i(&expires_in_days)),
            ]);
            let v = api.post("/v1/api-keys", body, Auth::ApiKey).await?;
            handle_key(ctx, v, key_out)
        }
        AppCmd::KeyUpdate { key_id, rate_limit } => ctx.print(
            &api.json(
                Method::PATCH,
                &format!("/v1/api-keys/{}", seg(&key_id)),
                &[],
                Some(&json!({ "rate_limit": rate_limit })),
                Auth::ApiKey,
            )
            .await?,
        ),
        AppCmd::KeyRevoke { key_id } => ctx.print(
            &api.delete(&format!("/v1/api-keys/{}", seg(&key_id)), &[], Auth::ApiKey)
                .await?,
        ),
        AppCmd::AuditLog { page, per_page } => {
            let q = query(vec![
                ("page", page.map(|v| v.to_string())),
                ("per_page", per_page.map(|v| v.to_string())),
            ]);
            ctx.print(&api.get("/v1/audit-log", &q, Auth::ApiKey).await?)
        }
    }
}
