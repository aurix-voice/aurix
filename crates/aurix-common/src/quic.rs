//! QUIC transport parameters shared by the media node and the native client (feature `quic`):
//! datagram-only connections, the connection-ID shape the node's shared-socket demultiplexer
//! relies on, certificate fingerprints and the client-side certificate pinning.
//!
//! Sealed AURX packets ride one per QUIC DATAGRAM; streams are disabled on both sides so a
//! lost datagram never delays the next one. The wire format inside the datagram is the UDP
//! one, so authentication, replay protection and E2EE do not depend on TLS at all.

use crate::error::{AurixError, Result};
use crate::protocol::{QuicInfo, MAX_PACKET_SIZE, QUIC_ALPN, TLS_TUNNEL_ALPN};
use quinn::rustls;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;

/// Connection IDs whose first byte has the high bit set; random otherwise. A QUIC short
/// header packet addressed to the node therefore never spells the AURX magic (`0x41 0x55`)
/// in its first two bytes, which lets the node share one UDP socket for both.
pub struct HighBitCidGenerator;

impl quinn::ConnectionIdGenerator for HighBitCidGenerator {
    fn generate_cid(&mut self) -> quinn::ConnectionId {
        let mut bytes = [0u8; 8];
        rand::Rng::fill(&mut rand::thread_rng(), &mut bytes);
        bytes[0] |= 0x80;
        quinn::ConnectionId::new(&bytes)
    }

    fn cid_len(&self) -> usize {
        8
    }

    fn cid_lifetime(&self) -> Option<Duration> {
        None
    }
}

/// Transport parameters: datagrams only, bounded send/receive queues of `queue_packets` AURX
/// packets, idle timeout, optional keep-alive (clients; the node relies on heartbeats).
pub fn transport_config(
    idle_timeout: Duration,
    queue_packets: usize,
    keep_alive: Option<Duration>,
) -> quinn::TransportConfig {
    let mut cfg = quinn::TransportConfig::default();
    let queue_bytes = queue_packets.max(1) * (MAX_PACKET_SIZE + 64);
    cfg.max_concurrent_bidi_streams(0u32.into());
    cfg.max_concurrent_uni_streams(0u32.into());
    cfg.datagram_receive_buffer_size(Some(queue_bytes));
    cfg.datagram_send_buffer_size(queue_bytes);
    cfg.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(idle_timeout).unwrap_or(quinn::VarInt::MAX.into()),
    ));
    cfg.keep_alive_interval(keep_alive);
    cfg.allow_spin(false);
    cfg
}

/// Endpoint parameters for both sides: our CID shape and no fixed-bit greasing (the node's
/// demultiplexer relies on the QUIC fixed bit being set).
pub fn endpoint_config() -> quinn::EndpointConfig {
    let mut cfg = quinn::EndpointConfig::default();
    cfg.cid_generator(|| Box::new(HighBitCidGenerator));
    cfg.grease_quic_bit(false);
    cfg
}

/// Lowercase hex SHA-256 of a DER certificate (what `QuicInfo::cert_sha256` carries).
pub fn cert_fingerprint(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

/// Client-side certificate check pinned to one node certificate: the fingerprint arrives over
/// the authenticated control channel, so no PKI is involved and no CA can impersonate a node.
#[derive(Debug)]
pub struct PinnedServerVerifier {
    cert_sha256: Vec<u8>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl PinnedServerVerifier {
    /// `cert_sha256` is the lowercase hex fingerprint from `QuicInfo`.
    pub fn new(cert_sha256: &str) -> Result<Arc<Self>> {
        let cert_sha256 = hex::decode(cert_sha256)
            .ok()
            .filter(|d| d.len() == 32)
            .ok_or_else(|| AurixError::Validation("invalid QUIC certificate fingerprint".into()))?;
        Ok(Arc::new(Self {
            cert_sha256,
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        }))
    }
}

impl rustls::client::danger::ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let digest = Sha256::digest(end_entity.as_ref());
        if digest.as_slice() == self.cert_sha256.as_slice() {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Client TLS configuration for one node: pinned certificate, our ALPN, early data enabled.
/// Resumption tickets live in this object, so reuse it for every connection to the same node
/// — that is what makes a 0-RTT reconnect possible at all.
pub fn client_tls_config(info: &QuicInfo) -> Result<Arc<rustls::ClientConfig>> {
    let verifier = PinnedServerVerifier::new(&info.cert_sha256)?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| AurixError::Internal(format!("QUIC TLS config: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    tls.alpn_protocols = vec![QUIC_ALPN.to_vec()];
    tls.enable_early_data = true;
    Ok(Arc::new(tls))
}

/// Client TLS configuration for the dedicated TLS media tunnel of one node: the same pinned
/// certificate check as [`client_tls_config`], TLS 1.3 only, ALPN [`TLS_TUNNEL_ALPN`].
pub fn tunnel_tls_config(cert_sha256: &str) -> Result<Arc<rustls::ClientConfig>> {
    let verifier = PinnedServerVerifier::new(cert_sha256)?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| AurixError::Internal(format!("TLS tunnel config: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    tls.alpn_protocols = vec![TLS_TUNNEL_ALPN.to_vec()];
    Ok(Arc::new(tls))
}

/// `quinn::ClientConfig` over a [`client_tls_config`] with our transport parameters.
pub fn client_config(
    tls: Arc<rustls::ClientConfig>,
    idle_timeout: Duration,
    queue_packets: usize,
    keep_alive: Option<Duration>,
) -> Result<quinn::ClientConfig> {
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .map_err(|e| AurixError::Internal(format!("QUIC crypto config: {e}")))?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(crypto));
    cfg.transport_config(Arc::new(transport_config(
        idle_timeout,
        queue_packets,
        keep_alive,
    )));
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use quinn::ConnectionIdGenerator;

    #[test]
    fn cids_never_look_like_aurx_magic() {
        let mut g = HighBitCidGenerator;
        for _ in 0..1000 {
            let cid = g.generate_cid();
            assert_eq!(cid.len(), 8);
            assert!(cid[0] & 0x80 != 0);
        }
    }

    #[test]
    fn fingerprint_is_hex_sha256() {
        let fp = cert_fingerprint(b"hello");
        assert_eq!(fp.len(), 64);
        assert_eq!(
            fp,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert!(PinnedServerVerifier::new(&fp).is_ok());
        assert!(PinnedServerVerifier::new("zz").is_err());
    }

    #[test]
    fn client_config_builds_from_advertised_info() {
        let info = QuicInfo {
            cert_sha256: cert_fingerprint(b"x"),
            server_name: "aurix-media".into(),
        };
        let tls = client_tls_config(&info).unwrap();
        assert!(tls.enable_early_data);
        assert_eq!(tls.alpn_protocols, vec![QUIC_ALPN.to_vec()]);
        client_config(
            tls,
            Duration::from_secs(10),
            64,
            Some(Duration::from_secs(3)),
        )
        .unwrap();
    }
}
