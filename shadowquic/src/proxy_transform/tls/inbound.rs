use std::net::SocketAddr;
use std::sync::Arc;

use rustls_jls::pki_types::CertificateDer;
use rustls_jls::pki_types::PrivateKeyDer;
use rustls_jls::pki_types::PrivatePkcs8KeyDer;
use serde::Deserialize;
use tokio_rustls_jls::LazyConfigAcceptor;
use tokio_rustls_jls::rustls as rustls_jls;
use tokio_rustls_jls::rustls::ServerConfig;

use crate::AnyTcp;
use crate::ProxyRequest;
use crate::TcpSession;
use crate::TcpTrait;
use crate::config::AuthUser;
use crate::config::JlsUpstream;
use crate::config::default_alpn;
use crate::config::default_zero_rtt;
use crate::error::SError;
use crate::proxy_transform::ProxyTransform;

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct JlsServerCfg {
    /// Users for client authentication
    pub users: Vec<AuthUser>,
    /// Server name used to check client. Must be the same as client
    /// If empty, server name will be parsed from jls_upstream
    /// If not available, server name check will be skipped
    pub server_name: Option<String>,
    /// Jls upstream, camouflage server, must be address with port. e.g.: `codepn.io:443`,`google.com:443`,`127.0.0.1:443`
    pub jls_upstream: JlsUpstream,
    /// Alpn of tls. Default is `["h3"]`, must have common element with client
    #[serde(default = "default_alpn")]
    pub alpn: Vec<String>,
    /// 0-RTT handshake.
    /// Set to true to enable zero rtt.
    /// Enabled by default
    #[serde(default = "default_zero_rtt")]
    pub zero_rtt: bool,
}

pub struct JlsServer {
    server_cfg: Arc<ServerConfig>,
}
impl JlsServer {
    pub fn new(cfg: JlsServerCfg) -> Result<Self, SError> {
        let _ = rustls_jls::crypto::ring::default_provider().install_default();

        let mut crypto: ServerConfig;
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_der = CertificateDer::from(cert.cert);
        let priv_key = PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
        crypto = ServerConfig::builder_with_protocol_versions(&[&rustls_jls::version::TLS13])
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], PrivateKeyDer::Pkcs8(priv_key))
            .map_err(|e| SError::RustlsError(e.to_string()))?;
        crypto.alpn_protocols = cfg
            .alpn
            .iter()
            .cloned()
            .map(|alpn| alpn.into_bytes())
            .collect();
        crypto.max_early_data_size = if cfg.zero_rtt { u32::MAX } else { 0 };
        crypto.send_half_rtt_data = cfg.zero_rtt;

        let mut jls_config = rustls_jls::jls::JlsServerConfig::default();
        for user in &cfg.users {
            jls_config = jls_config.add_user(user.password.clone(), user.username.clone());
        }
        if let Some(sni) = &cfg.server_name {
            jls_config = jls_config.with_server_name(sni.clone());
        }
        jls_config = jls_config
            .with_rate_limit(cfg.jls_upstream.rate_limit)
            .with_upstream_addr(cfg.jls_upstream.addr.clone())
            .enable(true);
        crypto.jls_config = jls_config.into();
        let server_cfg = Arc::new(crypto);
        Ok(Self { server_cfg })
    }
    pub async fn transform_tcp(&self, stream: AnyTcp) -> Result<AnyTcp, SError> {
        let conn = LazyConfigAcceptor::new(Default::default(), stream)
            .await
            .map_err(|e| SError::RustlsError(e.to_string()))?;

        let conn = conn.into_stream(self.server_cfg.clone());
        if !conn.is_jls() {
            conn.start_jls_forward().await?;
            return Err(SError::RustlsError("Jls forward ended".into()));
        }
        let conn = conn.await?;
        Ok(Box::new(conn))
    }
}

#[async_trait::async_trait]
impl ProxyTransform for JlsServer {
    async fn transform(&self, request: ProxyRequest) -> Result<ProxyRequest, SError> {
        match request {
            ProxyRequest::Tcp(mut session) => {
                session.stream = self.transform_tcp(session.stream).await?;
                Ok(ProxyRequest::Tcp(session))
            }
            p @ ProxyRequest::Udp(_) => Ok(p),
        }
    }
}

impl<S: TcpTrait> TcpTrait for tokio_rustls_jls::client::TlsStream<S> {}
impl<S: TcpTrait> TcpTrait for tokio_rustls_jls::server::TlsStream<S> {}
