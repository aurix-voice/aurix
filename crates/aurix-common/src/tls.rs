//! Outbound TLS for WebSocket clients (native SDK control channel, the API's live-stream proxy,
//! the recorder's live link): rustls on ring with the bundled Mozilla root store, so the trust
//! anchors do not depend on an OS certificate store (consoles and sandboxed runtimes have none
//! we can rely on) and come from a `webpki-roots` release we control, not from whatever the
//! WebSocket crate happens to pin.

use std::sync::{Arc, OnceLock};

/// Shared rustls client configuration: safe default protocol versions, Mozilla roots, no
/// client certificate. Built once per process.
pub fn mozilla_roots_client_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .expect("ring supports the default TLS versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_is_built_once_and_has_roots() {
        let a = mozilla_roots_client_config();
        let b = mozilla_roots_client_config();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!webpki_roots::TLS_SERVER_ROOTS.is_empty());
    }
}
