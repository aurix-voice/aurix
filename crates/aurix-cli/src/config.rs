//! Profiles: `~/.config/aurix/config.toml` (override with `AURIX_CONFIG`).
//!
//! Secrets are never stored in the config file itself — a profile points at a file
//! (`api_key_file`, `admin_token_file`) or an environment variable (`api_key_env`).
//! Resolution order for the API key: `--api-key-file`, `AURIX_API_KEY`, then the profile.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};

pub const DEFAULT_SERVER: &str = "http://localhost:8080";

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    /// Profile used when `--profile` / `AURIX_PROFILE` are absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Profile {
    /// Node HTTP origin, e.g. `https://voice.example.com`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// File containing the application API key (mode 0600 recommended).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_file: Option<PathBuf>,
    /// Environment variable holding the application API key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// File containing an operator (admin) JWT, written by `aurix admin login --save`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_token_file: Option<PathBuf>,
    /// Per-request timeout in seconds (default 15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

pub fn config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("AURIX_CONFIG") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("aurix").join("config.toml")
}

pub fn load(path: &Path) -> anyhow::Result<ConfigFile> {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ConfigFile::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn save(path: &Path, cfg: &ConfigFile) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let body = toml::to_string_pretty(cfg)?;
    write_private(path, body.as_bytes())
}

/// Writes `data` to `path` readable only by the current user (0600 on Unix).
pub fn write_private(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("writing {}", path.display()))?;
        f.write_all(data)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, data).with_context(|| format!("writing {}", path.display()))
    }
}

/// Reads a secret from a file, trimming the trailing newline editors add.
pub fn read_secret_file(path: &Path) -> anyhow::Result<String> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading secret file {}", path.display()))?;
    let s = raw.trim();
    if s.is_empty() {
        return Err(anyhow!("secret file {} is empty", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.permissions().mode() & 0o077 != 0 {
                eprintln!(
                    "warning: {} is readable by other users; chmod 600 it",
                    path.display()
                );
            }
        }
    }
    Ok(s.to_string())
}

/// Resolved, ready-to-use connection settings. Secrets are held in memory only.
#[derive(Debug)]
pub struct Resolved {
    pub profile_name: String,
    pub server: String,
    pub api_key: Option<String>,
    pub admin_token: Option<String>,
    pub timeout_secs: u64,
    /// Where the API key came from — for `config show`/`diagnose`, never the value.
    pub api_key_source: Option<String>,
    pub admin_token_source: Option<String>,
}

pub struct Overrides<'a> {
    pub profile: Option<&'a str>,
    pub server: Option<&'a str>,
    pub api_key_file: Option<&'a Path>,
    pub api_key_inline: Option<&'a str>,
    pub admin_token_file: Option<&'a Path>,
    pub timeout_secs: Option<u64>,
}

/// Environment lookup, injectable for tests.
pub type EnvFn<'a> = &'a dyn Fn(&str) -> Option<String>;

pub fn system_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

pub fn resolve(cfg: &ConfigFile, ov: &Overrides<'_>, env: EnvFn<'_>) -> anyhow::Result<Resolved> {
    let env_profile = env("AURIX_PROFILE");
    let profile_name = ov
        .profile
        .map(str::to_string)
        .or(env_profile)
        .or_else(|| cfg.default_profile.clone())
        .unwrap_or_else(|| "default".to_string());
    let profile = match cfg.profiles.get(&profile_name) {
        Some(p) => p.clone(),
        None if ov.profile.is_some() || cfg.default_profile.is_some() => {
            return Err(anyhow!(
                "profile '{profile_name}' not found in {}",
                config_path().display()
            ))
        }
        None => Profile::default(),
    };

    let server = ov
        .server
        .map(str::to_string)
        .or_else(|| env("AURIX_SERVER"))
        .or(profile.server)
        .unwrap_or_else(|| DEFAULT_SERVER.to_string());
    let server = server.trim_end_matches('/').to_string();
    if !(server.starts_with("http://") || server.starts_with("https://")) {
        return Err(anyhow!(
            "server must start with http:// or https:// (got '{server}')"
        ));
    }

    let (api_key, api_key_source) = if let Some(k) = ov.api_key_inline {
        eprintln!("warning: --api-key is visible to other processes; prefer --api-key-file or AURIX_API_KEY");
        (Some(k.to_string()), Some("--api-key".to_string()))
    } else if let Some(p) = ov.api_key_file {
        (
            Some(read_secret_file(p)?),
            Some(format!("--api-key-file {}", p.display())),
        )
    } else if let Some(k) = env("AURIX_API_KEY") {
        if k.trim().is_empty() {
            (None, None)
        } else {
            (
                Some(k.trim().to_string()),
                Some("env AURIX_API_KEY".to_string()),
            )
        }
    } else if let Some(p) = &profile.api_key_file {
        let p = expand_home(p);
        if p.is_file() {
            (
                Some(read_secret_file(&p)?),
                Some(format!("profile api_key_file {}", p.display())),
            )
        } else {
            (None, None)
        }
    } else if let Some(var) = &profile.api_key_env {
        match env(var) {
            Some(k) if !k.trim().is_empty() => {
                (Some(k.trim().to_string()), Some(format!("env {var}")))
            }
            _ => {
                return Err(anyhow!(
                    "profile '{profile_name}' expects the API key in ${var}, which is unset"
                ))
            }
        }
    } else {
        (None, None)
    };

    let (admin_token, admin_token_source) = if let Some(p) = ov.admin_token_file {
        (
            Some(read_secret_file(p)?),
            Some(format!("--admin-token-file {}", p.display())),
        )
    } else if let Some(t) = env("AURIX_ADMIN_TOKEN") {
        if t.trim().is_empty() {
            (None, None)
        } else {
            (
                Some(t.trim().to_string()),
                Some("env AURIX_ADMIN_TOKEN".to_string()),
            )
        }
    } else if let Some(p) = &profile.admin_token_file {
        let p = expand_home(p);
        match read_secret_file(&p) {
            Ok(t) => (
                Some(t),
                Some(format!("profile admin_token_file {}", p.display())),
            ),
            Err(_) => (None, None),
        }
    } else {
        let p = admin_token_path(&profile_name);
        match read_secret_file(&p) {
            Ok(t) if p.is_file() => (
                Some(t),
                Some(format!("saved by `admin login --save` {}", p.display())),
            ),
            _ => (None, None),
        }
    };

    Ok(Resolved {
        profile_name,
        server,
        api_key,
        admin_token,
        timeout_secs: ov.timeout_secs.or(profile.timeout_secs).unwrap_or(15),
        api_key_source,
        admin_token_source,
    })
}

pub fn expand_home(p: &Path) -> PathBuf {
    if let Ok(rest) = p.strip_prefix("~") {
        if let Some(home) = std::env::var_os("HOME") {
            return Path::new(&home).join(rest);
        }
    }
    p.to_path_buf()
}

/// Default location for a profile's saved admin token.
pub fn admin_token_path(profile: &str) -> PathBuf {
    config_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(format!("{profile}.admin-token"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn resolves_precedence_without_touching_env_secrets() {
        let mut cfg = ConfigFile::default();
        cfg.profiles.insert(
            "prod".into(),
            Profile {
                server: Some("https://voice.example.com/".into()),
                timeout_secs: Some(30),
                ..Profile::default()
            },
        );
        cfg.default_profile = Some("prod".into());
        let r = resolve(
            &cfg,
            &Overrides {
                profile: None,
                server: None,
                api_key_file: None,
                api_key_inline: None,
                admin_token_file: None,
                timeout_secs: None,
            },
            &no_env,
        )
        .unwrap();
        assert_eq!(r.profile_name, "prod");
        assert_eq!(r.server, "https://voice.example.com");
        assert_eq!(r.timeout_secs, 30);

        let r = resolve(
            &cfg,
            &Overrides {
                profile: None,
                server: Some("http://127.0.0.1:1"),
                api_key_file: None,
                api_key_inline: None,
                admin_token_file: None,
                timeout_secs: Some(3),
            },
            &no_env,
        )
        .unwrap();
        assert_eq!(r.server, "http://127.0.0.1:1");
        assert_eq!(r.timeout_secs, 3);

        let err = resolve(
            &cfg,
            &Overrides {
                profile: Some("missing"),
                server: None,
                api_key_file: None,
                api_key_inline: None,
                admin_token_file: None,
                timeout_secs: None,
            },
            &no_env,
        )
        .unwrap_err();
        assert!(err.to_string().contains("profile 'missing' not found"));
    }

    #[test]
    fn secret_file_is_trimmed_and_non_empty() {
        let dir = std::env::temp_dir().join(format!("aurix-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("key");
        write_private(&p, b"ak_live_abc\n").unwrap();
        assert_eq!(read_secret_file(&p).unwrap(), "ak_live_abc");
        write_private(&p, b"\n").unwrap();
        assert!(read_secret_file(&p).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_non_http_server() {
        let cfg = ConfigFile::default();
        let err = resolve(
            &cfg,
            &Overrides {
                profile: None,
                server: Some("voice.example.com"),
                api_key_file: None,
                api_key_inline: None,
                admin_token_file: None,
                timeout_secs: None,
            },
            &no_env,
        )
        .unwrap_err();
        assert!(err.to_string().contains("http://"));
    }

    #[test]
    fn profile_api_key_file_may_not_exist_yet() {
        let dir = std::env::temp_dir().join(format!("aurix-cli-missing-{}", std::process::id()));
        let mut cfg = ConfigFile::default();
        cfg.profiles.insert(
            "dev".into(),
            Profile {
                api_key_file: Some(dir.join("app.api-key")),
                ..Profile::default()
            },
        );
        cfg.default_profile = Some("dev".into());
        let ov = Overrides {
            profile: None,
            server: None,
            api_key_file: None,
            api_key_inline: None,
            admin_token_file: None,
            timeout_secs: None,
        };
        let r = resolve(&cfg, &ov, &no_env).unwrap();
        assert!(r.api_key.is_none() && r.api_key_source.is_none());

        std::fs::create_dir_all(&dir).unwrap();
        write_private(&dir.join("app.api-key"), b"aurx_test\n").unwrap();
        let r = resolve(&cfg, &ov, &no_env).unwrap();
        assert_eq!(r.api_key.as_deref(), Some("aurx_test"));
        let _ = std::fs::remove_dir_all(&dir);

        // An explicit --api-key-file that is missing is still an error.
        let missing = dir.join("nope");
        let err = resolve(
            &cfg,
            &Overrides {
                api_key_file: Some(&missing),
                ..ov
            },
            &no_env,
        )
        .unwrap_err();
        assert!(err.to_string().contains("reading secret file"));
    }
}
