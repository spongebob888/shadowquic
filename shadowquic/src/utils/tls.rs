use std::sync::Arc;

use crate::{TcpTrait, error::SError};

fn client_config() -> Result<rustls_jls::ClientConfig, SError> {
    let roots =
        rustls_jls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = rustls_jls::crypto::CryptoProvider::get_default()
        .cloned()
        .or_else(|| {
            #[cfg(feature = "ring")]
            {
                Some(Arc::new(rustls_jls::crypto::ring::default_provider()))
            }
            #[cfg(all(not(feature = "ring"), feature = "aws-lc-rs"))]
            {
                Some(Arc::new(rustls_jls::crypto::aws_lc_rs::default_provider()))
            }
            #[cfg(not(any(feature = "ring", feature = "aws-lc-rs")))]
            {
                None
            }
        })
        .ok_or_else(|| {
            SError::InvalidConfig(
                "TLS requires a rustls crypto provider (ring or aws-lc-rs)".into(),
            )
        })?;
    let mut config = rustls_jls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| SError::RustlsError(error.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.jls_config.enable = false;
    Ok(config)
}

/// Ordinary TLS with WebPKI roots and certificate verification, without JLS.
pub(crate) fn tls_connector() -> Result<tokio_rustls_jls::TlsConnector, SError> {
    let config = client_config()?;
    Ok(tokio_rustls_jls::TlsConnector::from(Arc::new(config)))
}

#[cfg(feature = "dns-server")]
pub(crate) fn quic_client_config() -> Result<quinn::rustls::ClientConfig, SError> {
    client_config()
}

impl TcpTrait for tokio_rustls_jls::client::TlsStream<tokio::io::DuplexStream> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_connector_disables_jls_and_early_data() {
        let connector = tls_connector().unwrap();
        assert!(!connector.config().jls_config.enable);
        assert!(!connector.config().enable_early_data);
    }

    #[cfg(feature = "dns-server")]
    #[test]
    fn quic_client_config_disables_jls_and_early_data() {
        let config = quic_client_config().unwrap();
        assert!(!config.jls_config.enable);
        assert!(!config.enable_early_data);
    }
}
