//! `X-Aurix-Signature` (`t=<unix>,v1=<hex hmac-sha256(secret, "<t>.<body>")>`) — mirrors
//! `aurix-control::webhooks` and the shared vector in `sdk/server/vectors`.

use hmac::{Hmac, Mac};
use sha2::Sha256;

pub fn sign(secret: &str, ts: i64, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length");
    mac.update(ts.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("t={ts},v1={}", hex::encode(mac.finalize().into_bytes()))
}

/// Returns the parsed timestamp on success so callers can report skew.
pub fn verify(
    secret: &str,
    header: &str,
    body: &[u8],
    now: i64,
    tolerance_secs: i64,
) -> Result<i64, &'static str> {
    let mut ts: Option<i64> = None;
    let mut v1: Option<&str> = None;
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", v)) => ts = v.parse().ok(),
            Some(("v1", v)) => v1 = Some(v),
            _ => {}
        }
    }
    let ts = ts.ok_or("missing or malformed t=")?;
    let v1 = v1.ok_or("missing v1=")?;
    if (now - ts).abs() > tolerance_secs {
        return Err("timestamp outside tolerance");
    }
    let got = hex::decode(v1).map_err(|_| "v1 is not hex")?;
    let expected = sign(secret, ts, body);
    let expected = hex::decode(&expected[expected.find("v1=").expect("sign() emits v1=") + 3..])
        .expect("own output is hex");
    if bool::from(subtle::ConstantTimeEq::ct_eq(
        got.as_slice(),
        expected.as_slice(),
    )) {
        Ok(ts)
    } else {
        Err("signature mismatch")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_shared_vector() {
        let v: serde_json::Value = serde_json::from_str(include_str!(
            "../../../sdk/server/vectors/webhook_signature.json"
        ))
        .unwrap();
        let secret = v["secret"].as_str().unwrap();
        let header = v["header"].as_str().unwrap();
        let body = v["body"].as_str().unwrap().as_bytes();
        let t = v["timestamp"].as_i64().unwrap();
        let tol = v["tolerance_sec"].as_i64().unwrap();
        assert_eq!(sign(secret, t, body), header);
        assert_eq!(verify(secret, header, body, t + 10, tol), Ok(t));
        assert!(verify(secret, header, b"x", t, tol).is_err());
        assert!(verify("other", header, body, t, tol).is_err());
        assert!(verify(secret, header, body, t + tol + 1, tol).is_err());
        assert!(verify(secret, "v1=zz", body, t, tol).is_err());
    }
}
