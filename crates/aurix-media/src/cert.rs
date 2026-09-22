//! The node's media-plane TLS identity, shared by the QUIC endpoint and the dedicated TLS
//! tunnel: an operator-provided PEM pair read once at start-up, or a self-signed certificate
//! generated for the configured server name. Clients never rely on a CA chain — the SHA-256
//! of the DER end-entity certificate travels over the authenticated control channel and is
//! pinned — so a self-signed certificate is exactly as strong as an issued one. Renewal (ACME
//! or otherwise) is not the node's job: restart it with the new files.

use aurix_common::quic::cert_fingerprint;
use aurix_common::{AurixError, Result};
use quinn::rustls;
use std::path::Path;

/// Certificate chain plus private key, and the pin clients check.
pub struct MediaCert {
    certs: Vec<rustls::pki_types::CertificateDer<'static>>,
    key: rustls::pki_types::PrivateKeyDer<'static>,
    fingerprint: String,
    server_name: String,
}

impl std::fmt::Debug for MediaCert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaCert")
            .field("fingerprint", &self.fingerprint)
            .field("server_name", &self.server_name)
            .field("chain_len", &self.certs.len())
            .finish()
    }
}

impl MediaCert {
    /// Reads the PEM pair at `cert_path` / `key_path`. Errors name the `media.quic_*` keys the
    /// operator set.
    pub fn load(cert_path: &Path, key_path: &Path, server_name: &str) -> Result<Self> {
        use rustls::pki_types::pem::PemObject;
        let cert_pem = std::fs::read(cert_path).map_err(|e| {
            AurixError::InvalidConfiguration(format!(
                "media.quic_cert_path {}: {e}",
                cert_path.display()
            ))
        })?;
        let key_pem = std::fs::read(key_path).map_err(|e| {
            AurixError::InvalidConfiguration(format!(
                "media.quic_key_path {}: {e}",
                key_path.display()
            ))
        })?;
        let certs = rustls::pki_types::CertificateDer::pem_slice_iter(&cert_pem)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| AurixError::InvalidConfiguration(format!("media.quic_cert_path: {e}")))?;
        if certs.is_empty() {
            return Err(AurixError::InvalidConfiguration(
                "media.quic_cert_path contains no certificate".into(),
            ));
        }
        let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(&key_pem)
            .map_err(|e| AurixError::InvalidConfiguration(format!("media.quic_key_path: {e}")))?;
        Ok(Self::new(certs, key, server_name))
    }

    /// Generates a self-signed certificate whose SAN is `server_name`.
    pub fn self_signed(server_name: &str) -> Result<Self> {
        let generated = rcgen::generate_simple_self_signed(vec![server_name.to_string()])
            .map_err(|e| AurixError::Internal(format!("media self-signed certificate: {e}")))?;
        let cert = generated.cert.der().clone();
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(generated.signing_key.serialize_der()),
        );
        Ok(Self::new(vec![cert], key, server_name))
    }

    /// The operator pair when configured, else a fresh self-signed certificate.
    pub fn load_or_generate(pair: Option<(&Path, &Path)>, server_name: &str) -> Result<Self> {
        match pair {
            Some((cert, key)) => Self::load(cert, key, server_name),
            None => Self::self_signed(server_name),
        }
    }

    fn new(
        certs: Vec<rustls::pki_types::CertificateDer<'static>>,
        key: rustls::pki_types::PrivateKeyDer<'static>,
        server_name: &str,
    ) -> Self {
        let fingerprint = cert_fingerprint(&certs[0]);
        Self {
            certs,
            key,
            fingerprint,
            server_name: server_name.to_string(),
        }
    }

    /// Lowercase hex SHA-256 of the DER end-entity certificate.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// SNI clients present / SAN of the generated certificate.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// A TLS 1.3-only rustls server configuration for this identity with `alpn` as the sole
    /// accepted protocol. Each transport calls this once and tunes the rest itself.
    pub fn server_config(&self, alpn: &[u8]) -> Result<rustls::ServerConfig> {
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let mut tls = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| AurixError::Internal(format!("media TLS config: {e}")))?
            .with_no_client_auth()
            .with_single_cert(self.certs.clone(), self.key.clone_key())
            .map_err(|e| AurixError::InvalidConfiguration(format!("media certificate: {e}")))?;
        tls.alpn_protocols = vec![alpn.to_vec()];
        Ok(tls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_signed_identity_has_a_pin_and_builds_server_configs() {
        let cert = MediaCert::self_signed("aurix-media").unwrap();
        assert_eq!(cert.fingerprint().len(), 64);
        assert_eq!(cert.server_name(), "aurix-media");
        let a = cert.server_config(b"a/1").unwrap();
        let b = cert.server_config(b"b/1").unwrap();
        assert_eq!(a.alpn_protocols, vec![b"a/1".to_vec()]);
        assert_eq!(b.alpn_protocols, vec![b"b/1".to_vec()]);
    }

    #[test]
    fn pem_pair_round_trips_with_the_same_fingerprint() {
        let generated = rcgen::generate_simple_self_signed(vec!["voice.example".into()]).unwrap();
        let dir = std::env::temp_dir().join(format!("aurix-cert-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        std::fs::write(&cert_path, generated.cert.pem()).unwrap();
        std::fs::write(&key_path, generated.signing_key.serialize_pem()).unwrap();
        let loaded = MediaCert::load(&cert_path, &key_path, "voice.example").unwrap();
        assert_eq!(
            loaded.fingerprint(),
            cert_fingerprint(generated.cert.der().as_ref())
        );
        assert!(MediaCert::load(&dir.join("missing.pem"), &key_path, "x").is_err());
        std::fs::write(&cert_path, b"not pem").unwrap();
        assert!(MediaCert::load(&cert_path, &key_path, "x").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
