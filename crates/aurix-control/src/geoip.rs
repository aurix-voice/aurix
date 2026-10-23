//! IP → approximate coordinates for region selection.
//!
//! Third source of a player's location after the game backend's `location` and the player's
//! own `latitude`/`longitude`: resolves the player's public IP against a MaxMind-format city
//! database (GeoLite2-City, DB-IP City Lite). Private, loopback and unspecified addresses
//! never resolve. The database is optional; a node without one simply orders regions by
//! preference and load, and the SDKs' RTT probe still picks the final node.

use arc_swap::ArcSwapOption;
use aurix_common::config::GeoIpConfig;
use aurix_common::types::GeoLocation;
use chrono::{Datelike, Utc};
use maxminddb::{geoip2, Reader};
use parking_lot::Mutex;
use std::io::Read;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Largest database accepted from a download (GeoLite2-City is ~60 MB, DB-IP City Lite ~130 MB).
const MAX_DOWNLOAD_BYTES: usize = 512 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct GeoHit {
    pub location: GeoLocation,
    /// ISO 3166-1 alpha-2 country code when the database carries one.
    pub country: Option<String>,
    /// Accuracy radius in km (MaxMind field); `None` for databases without it.
    pub accuracy_km: Option<u16>,
}

#[derive(Debug, Default)]
pub struct GeoIpStats {
    pub lookups: u64,
    pub hits: u64,
}

pub struct GeoIpResolver {
    reader: ArcSwapOption<Reader<Vec<u8>>>,
    path: Option<PathBuf>,
    loaded_mtime: Mutex<Option<SystemTime>>,
    lookups: AtomicU64,
    hits: AtomicU64,
}

impl std::fmt::Debug for GeoIpResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeoIpResolver")
            .field("path", &self.path)
            .field("loaded", &self.is_loaded())
            .finish()
    }
}

impl GeoIpResolver {
    /// No database: every lookup is `None`.
    pub fn disabled() -> Self {
        Self {
            reader: ArcSwapOption::empty(),
            path: None,
            loaded_mtime: Mutex::new(None),
            lookups: AtomicU64::new(0),
            hits: AtomicU64::new(0),
        }
    }

    /// Opens `database_path` when configured. A missing or unreadable file is logged, not
    /// fatal: the maintenance task (`spawn_maintenance`) downloads or picks it up later.
    pub fn from_config(cfg: &GeoIpConfig) -> Self {
        if !cfg.enabled() {
            return Self::disabled();
        }
        let path = PathBuf::from(cfg.database_path.trim());
        let resolver = Self {
            path: Some(path.clone()),
            ..Self::disabled()
        };
        match resolver.reload_if_changed() {
            Ok(true) => tracing::info!(path = %path.display(), "geoip database loaded"),
            Ok(false) => tracing::warn!(
                path = %path.display(),
                "geoip database not found yet; region selection falls back to hints and load"
            ),
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "geoip database unreadable")
            }
        }
        resolver
    }

    /// Opens a database file directly (tests, tools).
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let resolver = Self {
            path: Some(path.as_ref().to_path_buf()),
            ..Self::disabled()
        };
        if !resolver.reload_if_changed()? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} does not exist", path.as_ref().display()),
            ));
        }
        Ok(resolver)
    }

    pub fn is_loaded(&self) -> bool {
        self.reader.load().is_some()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn stats(&self) -> GeoIpStats {
        GeoIpStats {
            lookups: self.lookups.load(Ordering::Relaxed),
            hits: self.hits.load(Ordering::Relaxed),
        }
    }

    /// Reloads the file when its modification time differs from the loaded one. `Ok(false)`
    /// when the file does not exist (the previously loaded database, if any, stays in use).
    pub fn reload_if_changed(&self) -> std::io::Result<bool> {
        let Some(path) = self.path.as_deref() else {
            return Ok(false);
        };
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        let mtime = meta.modified().ok();
        if self.is_loaded() && *self.loaded_mtime.lock() == mtime {
            return Ok(true);
        }
        let reader = Reader::open_readfile(path)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        self.reader.store(Some(Arc::new(reader)));
        *self.loaded_mtime.lock() = mtime;
        Ok(true)
    }

    /// Coordinates (and country) for a public IP; `None` for private/unspecified addresses,
    /// addresses the database does not know, and records without coordinates.
    pub fn lookup(&self, ip: IpAddr) -> Option<GeoHit> {
        let reader = self.reader.load_full()?;
        if aurix_common::net::is_private_ip(ip) {
            return None;
        }
        self.lookups.fetch_add(1, Ordering::Relaxed);
        let result = reader.lookup(ip).ok()?;
        let city: geoip2::City = result.decode().ok()??;
        let location = GeoLocation {
            latitude: city.location.latitude?,
            longitude: city.location.longitude?,
        };
        if !location.is_valid() {
            return None;
        }
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(GeoHit {
            location,
            country: city.country.iso_code.map(str::to_owned),
            accuracy_km: city.location.accuracy_radius,
        })
    }

    pub fn location(&self, ip: IpAddr) -> Option<GeoLocation> {
        self.lookup(ip).map(|h| h.location)
    }

    /// Downloads the database when `download_url` is set and the file is missing or older
    /// than `refresh_hours`, validating it before it replaces the current file atomically.
    pub async fn refresh(&self, cfg: &GeoIpConfig, http: &reqwest::Client) -> anyhow::Result<bool> {
        let (Some(path), Some(url)) = (
            self.path.as_deref(),
            cfg.download_url
                .as_deref()
                .map(str::trim)
                .filter(|u| !u.is_empty()),
        ) else {
            return Ok(false);
        };
        let max_age = Duration::from_secs(cfg.refresh_hours.max(1) * 3600);
        if let Ok(meta) = std::fs::metadata(path) {
            if let Ok(age) = meta
                .modified()
                .and_then(|m| m.elapsed().map_err(std::io::Error::other))
            {
                if age < max_age {
                    return Ok(false);
                }
            }
        }
        let url = expand_url(url);
        let resp = http
            .get(&url)
            .timeout(Duration::from_secs(300))
            .send()
            .await?
            .error_for_status()?;
        let gz = url.split('?').next().is_some_and(|u| u.ends_with(".gz"))
            || resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("gzip"));
        let body = resp.bytes().await?;
        if body.len() > MAX_DOWNLOAD_BYTES {
            anyhow::bail!("geoip download larger than {MAX_DOWNLOAD_BYTES} bytes");
        }
        let bytes = if gz || body.starts_with(&[0x1f, 0x8b]) {
            let mut out = Vec::with_capacity(body.len() * 4);
            flate2::read::GzDecoder::new(&body[..])
                .take(MAX_DOWNLOAD_BYTES as u64 + 1)
                .read_to_end(&mut out)?;
            if out.len() > MAX_DOWNLOAD_BYTES {
                anyhow::bail!("geoip database larger than {MAX_DOWNLOAD_BYTES} bytes");
            }
            out
        } else {
            body.to_vec()
        };
        Reader::from_source(bytes.clone())
            .map_err(|e| anyhow::anyhow!("downloaded geoip database is invalid: {e}"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("mmdb.tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)?;
        self.reload_if_changed()?;
        tracing::info!(path = %path.display(), bytes = bytes.len(), "geoip database downloaded");
        Ok(true)
    }

    /// Background task: downloads on start when needed, then watches the file for external
    /// updates every `reload_check_secs` and re-downloads every `refresh_hours`.
    pub fn spawn_maintenance(self: &Arc<Self>, cfg: GeoIpConfig, http: reqwest::Client) {
        if self.path.is_none() {
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let check = Duration::from_secs(cfg.reload_check_secs.clamp(10, 86_400));
            loop {
                match me.refresh(&cfg, &http).await {
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "geoip database download failed"),
                }
                if let Err(e) = me.reload_if_changed() {
                    tracing::warn!(error = %e, "geoip database reload failed");
                }
                tokio::time::sleep(check).await;
            }
        });
    }
}

fn expand_url(url: &str) -> String {
    let now = Utc::now();
    url.replace("{YYYY}", &format!("{:04}", now.year()))
        .replace("{MM}", &format!("{:02}", now.month()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> GeoIpResolver {
        GeoIpResolver::open(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/geoip-test.mmdb"
        ))
        .expect("fixture opens")
    }

    #[test]
    fn resolves_v4_and_v6_with_country() {
        let g = fixture();
        let hit = g.lookup("89.160.20.128".parse().unwrap()).expect("hit");
        assert_eq!(hit.country.as_deref(), Some("SE"));
        assert!((hit.location.latitude - 58.4167).abs() < 1e-3);
        assert_eq!(hit.accuracy_km, Some(50));
        let v6 = g
            .location("2a02:aa08:1::1".parse().unwrap())
            .expect("v6 hit");
        assert!((v6.longitude - 18.0687).abs() < 1e-3);
        assert_eq!(g.stats().hits, 2);
    }

    #[test]
    fn unknown_private_and_coordinate_less_records_miss() {
        let g = fixture();
        assert!(g.lookup("8.8.8.8".parse().unwrap()).is_none());
        assert!(g.lookup("10.1.2.3".parse().unwrap()).is_none());
        assert!(g.lookup("127.0.0.1".parse().unwrap()).is_none());
        // Country-only record: no coordinates, so no location.
        assert!(g.lookup("203.0.113.9".parse().unwrap()).is_none());
        assert!(GeoIpResolver::disabled()
            .lookup("89.160.20.128".parse().unwrap())
            .is_none());
    }

    #[test]
    fn missing_file_is_tolerated_and_picked_up_later() {
        let dir = std::env::temp_dir().join(format!("aurix-geoip-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("city.mmdb");
        let cfg = GeoIpConfig {
            database_path: path.to_string_lossy().into_owned(),
            ..GeoIpConfig::default()
        };
        let g = GeoIpResolver::from_config(&cfg);
        assert!(!g.is_loaded());
        std::fs::copy(
            concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/geoip-test.mmdb"),
            &path,
        )
        .unwrap();
        assert!(g.reload_if_changed().unwrap());
        assert!(g.location("89.160.20.1".parse().unwrap()).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn url_placeholders_expand() {
        let u = expand_url("https://x/dbip-city-lite-{YYYY}-{MM}.mmdb.gz");
        assert!(!u.contains('{'));
        assert_eq!(u.len(), "https://x/dbip-city-lite-2026-10.mmdb.gz".len());
    }
}
