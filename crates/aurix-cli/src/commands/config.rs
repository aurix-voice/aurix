use std::path::PathBuf;

use anyhow::{anyhow, bail};
use clap::{Args as ClapArgs, Subcommand};
use serde_json::json;

use crate::config::{self as cfg, ConfigFile, Profile};
use crate::output::Printer;

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create or update a profile (secrets are referenced, never stored).
    Init {
        /// Profile name.
        #[arg(long, default_value = "default")]
        name: String,
        /// Node HTTP origin.
        #[arg(long)]
        server: Option<String>,
        /// File holding the application API key (chmod 600).
        #[arg(long, value_name = "PATH")]
        api_key_file: Option<PathBuf>,
        /// Environment variable holding the application API key.
        #[arg(long, value_name = "VAR")]
        api_key_env: Option<String>,
        /// File holding an operator JWT (`aurix admin login --save` writes it).
        #[arg(long, value_name = "PATH")]
        admin_token_file: Option<PathBuf>,
        #[arg(long)]
        timeout: Option<u64>,
        /// Make this the default profile.
        #[arg(long)]
        set_default: bool,
    },
    /// Show the effective configuration (credential *sources*, not values).
    Show,
    /// Print the config file path.
    Path,
    /// List profiles.
    Profiles,
    /// Set the default profile.
    Use { name: String },
    /// Remove a profile (does not touch secret files).
    Remove { name: String },
}

pub fn run(args: &Args, file: &ConfigFile, out: &Printer) -> anyhow::Result<()> {
    let path = cfg::config_path();
    match &args.cmd {
        Cmd::Init {
            name,
            server,
            api_key_file,
            api_key_env,
            admin_token_file,
            timeout,
            set_default,
        } => {
            if api_key_file.is_some() && api_key_env.is_some() {
                bail!("use either --api-key-file or --api-key-env");
            }
            let mut file = file.clone();
            let p = file.profiles.entry(name.clone()).or_default();
            if let Some(s) = server {
                if !(s.starts_with("http://") || s.starts_with("https://")) {
                    bail!("--server must start with http:// or https://");
                }
                p.server = Some(s.trim_end_matches('/').to_string());
            }
            if let Some(f) = api_key_file {
                let abs = if f.is_absolute() {
                    f.clone()
                } else {
                    std::env::current_dir()?.join(f)
                };
                if !abs.exists() {
                    eprintln!(
                        "note: {} does not exist yet; create it with mode 600",
                        abs.display()
                    );
                }
                p.api_key_file = Some(abs);
                p.api_key_env = None;
            }
            if let Some(v) = api_key_env {
                p.api_key_env = Some(v.clone());
                p.api_key_file = None;
            }
            if let Some(f) = admin_token_file {
                p.admin_token_file = Some(if f.is_absolute() {
                    f.clone()
                } else {
                    std::env::current_dir()?.join(f)
                });
            }
            if let Some(t) = timeout {
                p.timeout_secs = Some(*t);
            }
            if *set_default || file.default_profile.is_none() {
                file.default_profile = Some(name.clone());
            }
            cfg::save(&path, &file)?;
            out.print(&json!({ "config": path, "profile": name, "default_profile": file.default_profile }))
        }
        Cmd::Show => {
            let default = file
                .default_profile
                .clone()
                .unwrap_or_else(|| "default".into());
            let mut profiles = serde_json::Map::new();
            for (name, p) in &file.profiles {
                profiles.insert(name.clone(), describe(name, p));
            }
            if profiles.is_empty() {
                profiles.insert("default".into(), describe("default", &Profile::default()));
            }
            out.print(&json!({
                "config": path,
                "exists": path.exists(),
                "default_profile": default,
                "env": {
                    "AURIX_PROFILE": std::env::var_os("AURIX_PROFILE").is_some(),
                    "AURIX_SERVER": std::env::var("AURIX_SERVER").ok(),
                    "AURIX_API_KEY": std::env::var_os("AURIX_API_KEY").is_some(),
                    "AURIX_ADMIN_TOKEN": std::env::var_os("AURIX_ADMIN_TOKEN").is_some(),
                },
                "profiles": profiles,
            }))
        }
        Cmd::Path => {
            println!("{}", path.display());
            Ok(())
        }
        Cmd::Profiles => {
            let names: Vec<&String> = file.profiles.keys().collect();
            out.print(&json!({ "default_profile": file.default_profile, "profiles": names }))
        }
        Cmd::Use { name } => {
            if !file.profiles.contains_key(name) {
                return Err(anyhow!(
                    "profile '{name}' does not exist (aurix config init --name {name} ...)"
                ));
            }
            let mut file = file.clone();
            file.default_profile = Some(name.clone());
            cfg::save(&path, &file)?;
            out.print(&json!({ "default_profile": name }))
        }
        Cmd::Remove { name } => {
            let mut file = file.clone();
            if file.profiles.remove(name).is_none() {
                return Err(anyhow!("profile '{name}' does not exist"));
            }
            if file.default_profile.as_deref() == Some(name) {
                file.default_profile = None;
            }
            cfg::save(&path, &file)?;
            out.print(&json!({ "removed": name }))
        }
    }
}

fn describe(name: &str, p: &Profile) -> serde_json::Value {
    let saved = cfg::admin_token_path(name);
    json!({
        "server": p.server.clone().unwrap_or_else(|| cfg::DEFAULT_SERVER.to_string()),
        "api_key": match (&p.api_key_file, &p.api_key_env) {
            (Some(f), _) => json!({ "source": "file", "path": f, "present": cfg::expand_home(f).exists() }),
            (None, Some(v)) => json!({ "source": "env", "var": v, "present": std::env::var_os(v).is_some() }),
            _ => json!({ "source": "none" }),
        },
        "admin_token": match &p.admin_token_file {
            Some(f) => json!({ "source": "file", "path": f, "present": cfg::expand_home(f).exists() }),
            None if saved.is_file() => json!({ "source": "login", "path": saved, "present": true }),
            None => json!({ "source": "none" }),
        },
        "timeout_secs": p.timeout_secs.unwrap_or(15),
    })
}
