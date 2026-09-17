//! Minimal AWS Signature V4 client for S3-compatible object stores (PUT / DELETE / presigned GET).

use aurix_common::error::{AurixError, Result};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::time::Duration;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone)]
pub struct S3Config {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    /// Path-style addressing (`endpoint/bucket/key`), required by MinIO and most self-hosted stores.
    pub path_style: bool,
}

pub struct S3Client {
    cfg: S3Config,
    http: reqwest::Client,
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// RFC 3986 unreserved-character encoding used by SigV4 for URI paths and query strings.
fn uri_encode(input: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

impl S3Client {
    pub fn new(cfg: S3Config) -> Result<Self> {
        if cfg.access_key.is_empty() || cfg.secret_key.is_empty() {
            return Err(AurixError::InvalidConfiguration("S3 access/secret key required".into()));
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| AurixError::Internal(format!("http client: {e}")))?;
        Ok(Self { cfg, http })
    }

    fn host(&self) -> Result<(String, String)> {
        let url = url::Url::parse(&self.cfg.endpoint)
            .map_err(|e| AurixError::InvalidConfiguration(format!("invalid s3 endpoint: {e}")))?;
        let scheme = url.scheme().to_string();
        let mut host = url.host_str().ok_or_else(|| AurixError::InvalidConfiguration("s3 endpoint has no host".into()))?.to_string();
        if let Some(p) = url.port() {
            host = format!("{host}:{p}");
        }
        if !self.cfg.path_style {
            host = format!("{}.{}", self.cfg.bucket, host);
        }
        Ok((scheme, host))
    }

    fn canonical_path(&self, key: &str) -> String {
        if self.cfg.path_style {
            format!("/{}/{}", uri_encode(&self.cfg.bucket, true), uri_encode(key, false))
        } else {
            format!("/{}", uri_encode(key, false))
        }
    }

    fn signing_key(&self, date: &str) -> Vec<u8> {
        let k_date = hmac(format!("AWS4{}", self.cfg.secret_key).as_bytes(), date.as_bytes());
        let k_region = hmac(&k_date, self.cfg.region.as_bytes());
        let k_service = hmac(&k_region, b"s3");
        hmac(&k_service, b"aws4_request")
    }

    #[allow(clippy::too_many_arguments)]
    fn sign(
        &self,
        method: &str,
        path: &str,
        query: &str,
        host: &str,
        now: DateTime<Utc>,
        payload_hash: &str,
        extra_headers: &[(&str, &str)],
    ) -> (String, String) {
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = now.format("%Y%m%d").to_string();

        let mut headers: Vec<(String, String)> = vec![
            ("host".into(), host.to_string()),
            ("x-amz-content-sha256".into(), payload_hash.to_string()),
            ("x-amz-date".into(), amz_date.clone()),
        ];
        for (k, v) in extra_headers {
            headers.push((k.to_lowercase(), v.trim().to_string()));
        }
        headers.sort();
        let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
        let signed_headers = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");

        let canonical_request = format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
        let scope = format!("{date}/{}/s3/aws4_request", self.cfg.region);
        let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(canonical_request.as_bytes()));
        let signature = hex::encode(hmac(&self.signing_key(&date), string_to_sign.as_bytes()));
        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.cfg.access_key
        );
        (auth, amz_date)
    }

    pub async fn put_object(&self, key: &str, body: Vec<u8>, content_type: &str) -> Result<()> {
        let (scheme, host) = self.host()?;
        let path = self.canonical_path(key);
        let payload_hash = sha256_hex(&body);
        let (auth, amz_date) = self.sign("PUT", &path, "", &host, Utc::now(), &payload_hash, &[("content-type", content_type)]);
        let url = format!("{scheme}://{host}{path}");
        let resp = self
            .http
            .put(&url)
            .header("Host", &host)
            .header("Authorization", auth)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", payload_hash)
            .header("Content-Type", content_type)
            .body(body)
            .send()
            .await
            .map_err(|e| AurixError::Recording(format!("S3 PUT failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(AurixError::Recording(format!("S3 PUT returned {}", resp.status())));
        }
        Ok(())
    }

    pub async fn delete_object(&self, key: &str) -> Result<()> {
        let (scheme, host) = self.host()?;
        let path = self.canonical_path(key);
        let payload_hash = sha256_hex(b"");
        let (auth, amz_date) = self.sign("DELETE", &path, "", &host, Utc::now(), &payload_hash, &[]);
        let url = format!("{scheme}://{host}{path}");
        let resp = self
            .http
            .delete(&url)
            .header("Host", &host)
            .header("Authorization", auth)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", payload_hash)
            .send()
            .await
            .map_err(|e| AurixError::Recording(format!("S3 DELETE failed: {e}")))?;
        if !resp.status().is_success() && resp.status().as_u16() != 404 {
            return Err(AurixError::Recording(format!("S3 DELETE returned {}", resp.status())));
        }
        Ok(())
    }

    /// Build a presigned GET URL valid for `expires_secs` (max 7 days per AWS).
    pub fn presigned_get_url(&self, key: &str, expires_secs: u64, now: DateTime<Utc>) -> Result<String> {
        let (scheme, host) = self.host()?;
        let path = self.canonical_path(key);
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = now.format("%Y%m%d").to_string();
        let scope = format!("{date}/{}/s3/aws4_request", self.cfg.region);
        let credential = uri_encode(&format!("{}/{scope}", self.cfg.access_key), true);
        let expires = expires_secs.clamp(1, 604_800);
        let query = format!(
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential={credential}&X-Amz-Date={amz_date}&X-Amz-Expires={expires}&X-Amz-SignedHeaders=host"
        );
        let canonical_request = format!("GET\n{path}\n{query}\nhost:{host}\n\nhost\nUNSIGNED-PAYLOAD");
        let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(canonical_request.as_bytes()));
        let signature = hex::encode(hmac(&self.signing_key(&date), string_to_sign.as_bytes()));
        Ok(format!("{scheme}://{host}{path}?{query}&X-Amz-Signature={signature}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn client() -> S3Client {
        S3Client::new(S3Config {
            endpoint: "https://s3.amazonaws.com".into(),
            region: "us-east-1".into(),
            bucket: "examplebucket".into(),
            access_key: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            path_style: false,
        })
        .unwrap()
    }

    /// AWS documentation example: "Example: Presigned URL" in the SigV4 S3 guide.
    #[test]
    fn presigned_url_matches_aws_reference_vector() {
        let now = Utc.with_ymd_and_hms(2013, 5, 24, 0, 0, 0).unwrap();
        let url = client().presigned_get_url("test.txt", 86400, now).unwrap();
        assert!(url.starts_with("https://examplebucket.s3.amazonaws.com/test.txt?"));
        assert!(url.contains("X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request"));
        assert!(url.ends_with("X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"));
    }

    #[test]
    fn uri_encoding_keeps_slashes_in_paths_only() {
        assert_eq!(uri_encode("a b/c+d", false), "a%20b/c%2Bd");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
    }
}
