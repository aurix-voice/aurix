//! Region discovery for native clients.
//!
//! The server advertises, per region, the least-loaded healthy node that has a public
//! WebSocket URL (`GET /v1/me/regions`, or `endpoint` in the `POST /v1/tokens` response). Session
//! resume is node-local, so a client should connect to a node's *direct* URL and keep it.
//!
//! The core deliberately has no HTTP client (engines ship their own): the host fetches the JSON
//! and measures RTT to each `probe_url`; this module parses the response, builds the request
//! URL and ranks the result. Geography is only a proxy for latency, so a measured RTT overrides
//! the server's distance order; ties keep that order.

use serde::Deserialize;

pub use aurix_common::types::{GeoLocation, Region, RegionEndpoint};

use crate::error::{ClientError, Result};

/// RTT differences below this are treated as ties (network noise, not a better region).
pub const DEFAULT_RTT_TOLERANCE_MS: f64 = 15.0;

/// Body of `GET /v1/regions` / `GET /v1/me/regions`.
#[derive(Debug, Clone, Deserialize)]
pub struct RegionsResponse {
    /// Server order: preferred region first, then distance (when a location was sent), then load.
    pub regions: Vec<RegionEndpoint>,
    /// First entry of `regions`, if any.
    pub recommended: Option<RegionEndpoint>,
}

/// A region with the host's RTT measurement attached.
#[derive(Debug, Clone)]
pub struct ProbedRegion {
    pub endpoint: RegionEndpoint,
    /// Best sample in milliseconds; `None` when not probed or unreachable.
    pub rtt_ms: Option<f64>,
    /// A probe was attempted and every request failed.
    pub probe_failed: bool,
}

impl ProbedRegion {
    pub fn unprobed(endpoint: RegionEndpoint) -> Self {
        Self {
            endpoint,
            rtt_ms: None,
            probe_failed: false,
        }
    }

    /// Record a probe outcome: `Some(rtt)` for the best successful sample, `None` for unreachable.
    pub fn with_rtt(mut self, rtt_ms: Option<f64>) -> Self {
        self.set_rtt(rtt_ms);
        self
    }

    pub fn set_rtt(&mut self, rtt_ms: Option<f64>) {
        self.rtt_ms = rtt_ms.filter(|r| r.is_finite() && *r >= 0.0);
        self.probe_failed = self.rtt_ms.is_none();
    }
}

/// Parse a region name as the API spells it (`us_east`, `eu-west`, `AsiaPacific`...); `None` for
/// unknown names (unlike `Region::from_str_loose`, which falls back to `us_east`).
pub fn parse_region(name: &str) -> Option<Region> {
    let wanted = name.trim().to_lowercase().replace(['-', '_'], "");
    Region::all()
        .iter()
        .copied()
        .find(|r| r.as_str().replace('-', "") == wanted)
}

/// Parse a discovery response body.
pub fn parse_regions(json: &str) -> Result<RegionsResponse> {
    serde_json::from_str(json)
        .map_err(|e| ClientError::Protocol(format!("malformed regions response: {e}")))
}

/// `GET` URL for player-scoped discovery with optional region/location hints.
pub fn discovery_url(
    api_url: &str,
    preferred: Option<Region>,
    location: Option<GeoLocation>,
) -> String {
    let mut url = format!("{}/v1/me/regions", api_url.trim_end_matches('/'));
    let mut sep = '?';
    if let Some(region) = preferred {
        url.push(sep);
        url.push_str("region=");
        url.push_str(&region.as_str().replace('-', "_"));
        sep = '&';
    }
    if let Some(loc) = location.filter(GeoLocation::is_valid) {
        url.push_str(&format!(
            "{sep}latitude={}&longitude={}",
            loc.latitude, loc.longitude
        ));
    }
    url
}

/// Stable re-ranking of the server's list: the preferred region first (unless its probe failed),
/// then measured regions in ascending `tolerance_ms` buckets, then regions that were not probed,
/// and last regions whose probe failed. Within a bucket the server order (distance, load) is kept.
pub fn rank_regions(
    mut regions: Vec<ProbedRegion>,
    preferred: Option<Region>,
    tolerance_ms: f64,
) -> Vec<ProbedRegion> {
    let tolerance = if tolerance_ms.is_finite() && tolerance_ms >= 1.0 {
        tolerance_ms
    } else {
        1.0
    };
    let key = |r: &ProbedRegion| -> i64 {
        let is_preferred = preferred == Some(r.endpoint.region);
        if is_preferred && (r.rtt_ms.is_some() || !r.probe_failed) {
            return -1;
        }
        match r.rtt_ms {
            Some(rtt) => (rtt / tolerance).floor() as i64,
            None if r.probe_failed => i64::MAX,
            None => i64::MAX - 1,
        }
    };
    regions.sort_by_key(key);
    regions
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn ep(region: Region, node: &str, probe: bool) -> RegionEndpoint {
        RegionEndpoint {
            region,
            node_id: aurix_common::types::MediaNodeId(Uuid::new_v4()),
            ws_url: format!("wss://{node}.example/ws"),
            probe_url: probe.then(|| format!("https://{node}.example/health")),
            location: None,
            distance_km: None,
            nodes: 1,
            load_factor: 0.1,
        }
    }

    fn names(v: &[ProbedRegion]) -> Vec<&str> {
        v.iter()
            .map(|r| {
                r.endpoint
                    .ws_url
                    .trim_start_matches("wss://")
                    .split('.')
                    .next()
                    .unwrap()
            })
            .collect()
    }

    fn fixture() -> Vec<ProbedRegion> {
        vec![
            ProbedRegion::unprobed(ep(Region::EuWest, "eu1", true)).with_rtt(Some(40.0)),
            ProbedRegion::unprobed(ep(Region::EuCentral, "eu2", true)).with_rtt(Some(30.0)),
            ProbedRegion::unprobed(ep(Region::UsEast, "us1", true)).with_rtt(Some(120.0)),
            ProbedRegion::unprobed(ep(Region::Africa, "af1", false)),
        ]
    }

    #[test]
    fn preferred_then_rtt_buckets_then_unmeasured() {
        assert_eq!(
            names(&rank_regions(fixture(), None, DEFAULT_RTT_TOLERANCE_MS)),
            ["eu1", "eu2", "us1", "af1"]
        );
        assert_eq!(
            names(&rank_regions(fixture(), None, 5.0)),
            ["eu2", "eu1", "us1", "af1"]
        );
        assert_eq!(
            names(&rank_regions(fixture(), Some(Region::UsEast), 15.0)),
            ["us1", "eu1", "eu2", "af1"]
        );
        assert_eq!(
            names(&rank_regions(fixture(), Some(Region::Africa), 15.0)),
            ["af1", "eu1", "eu2", "us1"]
        );
        let mut dead = fixture();
        dead[2].set_rtt(None);
        assert!(dead[2].probe_failed);
        assert_eq!(
            names(&rank_regions(dead, Some(Region::UsEast), 15.0)),
            ["eu1", "eu2", "af1", "us1"]
        );
    }

    #[test]
    fn parses_wire_shape_and_builds_urls() {
        let id = Uuid::new_v4();
        let body = format!(
            r#"{{"regions":[{{"region":"eu_west","node_id":"{id}","ws_url":"wss://eu1/ws","probe_url":"https://eu1/health","location":{{"latitude":48.8,"longitude":2.3}},"distance_km":12.5,"nodes":2,"load_factor":0.25}},
                {{"region":"africa","node_id":"{}","ws_url":"wss://af/ws","probe_url":null,"location":null,"distance_km":null,"nodes":1,"load_factor":0.0}}],
                "recommended":null}}"#,
            Uuid::new_v4()
        );
        let parsed = parse_regions(&body).unwrap();
        assert_eq!(parsed.regions.len(), 2);
        assert_eq!(parsed.regions[0].region, Region::EuWest);
        assert_eq!(parsed.regions[0].node_id.0, id);
        assert_eq!(parsed.regions[0].distance_km, Some(12.5));
        assert_eq!(parsed.regions[0].location.unwrap().latitude, 48.8);
        assert!(parsed.regions[1].probe_url.is_none());
        assert!(parsed.recommended.is_none());
        assert!(parse_regions("{}").is_err());

        assert_eq!(parse_region("us_east"), Some(Region::UsEast));
        assert_eq!(parse_region("eu-west"), Some(Region::EuWest));
        assert_eq!(parse_region("AsiaPacific"), Some(Region::AsiaPacific));
        assert_eq!(parse_region("mars"), None);
        assert_eq!(
            discovery_url("https://api/", None, None),
            "https://api/v1/me/regions"
        );
        assert_eq!(
            discovery_url(
                "https://api",
                Some(Region::UsEast),
                Some(GeoLocation {
                    latitude: 1.5,
                    longitude: -2.0
                })
            ),
            "https://api/v1/me/regions?region=us_east&latitude=1.5&longitude=-2"
        );
        assert_eq!(
            discovery_url(
                "https://api",
                None,
                Some(GeoLocation {
                    latitude: 91.0,
                    longitude: 0.0
                })
            ),
            "https://api/v1/me/regions"
        );
    }
}
