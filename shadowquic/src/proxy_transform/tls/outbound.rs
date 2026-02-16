use std::sync::Arc;

use rustls_jls::ClientConfig;
use rustls_jls::RootCertStore;
use rustls_jls::pki_types::CertificateDer;
use rustls_jls::pki_types::PrivateKeyDer;
use rustls_jls::pki_types::PrivatePkcs8KeyDer;
use serde::Deserialize;
use tokio_rustls_jls::LazyConfigAcceptor;
use tokio_rustls_jls::TlsConnector;
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
use crate::error::SResult;
use crate::proxy_transform::ProxyTransform;

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct JlsClientCfg {
    /// username, must be the same as the server
    pub username: String,
    /// password, must be the same as the server
    pub password: String,
    /// Server name, must be the same as the server jls_upstream
    /// domain name
    pub server_name: String,
    /// Alpn of tls, default is \["h3"\], must have common element with server
    #[serde(default = "default_alpn")]
    pub alpn: Vec<String>,
    /// Set to true to enable zero rtt, default to true
    #[serde(default = "default_zero_rtt")]
    pub zero_rtt: bool,

    /// Android Only. the unix socket path for protecting android socket
    #[cfg(target_os = "android")]
    #[serde(default)]
    pub protect_path: Option<std::path::PathBuf>,
}

pub struct JlsClient {
    cfg: Arc<ClientConfig>,
    server_name: String,
}

impl JlsClient {
    pub fn new(cfg: JlsClientCfg) -> Self {
        let root_store = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.into(),
        };
        let mut crypto = rustls_jls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        crypto.alpn_protocols = cfg.alpn.iter().map(|x| x.to_owned().into_bytes()).collect();
        crypto.enable_early_data = cfg.zero_rtt;
        crypto.jls_config = rustls_jls::jls::JlsClientConfig::new(&cfg.password, &cfg.username);
        Self {
            cfg: Arc::new(crypto),
            server_name: cfg.server_name,
        }
    }
    pub async fn transform_tcp(&self, stream: AnyTcp) -> SResult<AnyTcp> {
        let connector = TlsConnector::from(self.cfg.clone()).early_data(self.cfg.enable_early_data);
        let conn = connector
            .connect(self.server_name.clone().try_into().unwrap(), stream)
            .await?;

        Ok(Box::new(conn))
    }
}

#[async_trait::async_trait]
impl ProxyTransform for JlsClient {
    async fn transform(&self, proxy: ProxyRequest) -> SResult<ProxyRequest> {
        match proxy {
            ProxyRequest::Tcp(mut session) => {
                session.stream = self.transform_tcp(session.stream).await?;
                Ok(ProxyRequest::Tcp(session))
            }
            p @ ProxyRequest::Udp(_) => Ok(p),
        }
    }
}
