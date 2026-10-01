use std::net::SocketAddr;

use serde::Deserialize;

use super::{Backend, DnsServer, tls_connector};
use crate::error::SError;

/// Local UDP/TCP DNS listener using a UDP upstream, with TCP fallback.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct DnsUdpServerCfg {
    pub tag: String,
    pub bind_addr: SocketAddr,
    /// Literal upstream address avoids recursive bootstrap resolution.
    pub upstream: SocketAddr,
}

impl DnsUdpServerCfg {
    pub async fn build(self) -> Result<DnsServer, SError> {
        DnsServer::new(self.tag, self.bind_addr, Backend::Udp(self.upstream)).await
    }
}

/// Local UDP/TCP DNS listener using a TCP upstream.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct DnsTcpServerCfg {
    pub tag: String,
    pub bind_addr: SocketAddr,
    pub upstream: SocketAddr,
}

impl DnsTcpServerCfg {
    pub async fn build(self) -> Result<DnsServer, SError> {
        DnsServer::new(self.tag, self.bind_addr, Backend::Tcp(self.upstream)).await
    }
}

/// Local UDP/TCP DNS listener using a certificate-verified TLS upstream.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct DnsTlsServerCfg {
    pub tag: String,
    pub bind_addr: SocketAddr,
    pub upstream: SocketAddr,
    /// TLS identity verified against the public root store.
    pub server_name: String,
}

impl DnsTlsServerCfg {
    pub fn validate(&self) -> Result<(), SError> {
        rustls::pki_types::ServerName::try_from(self.server_name.as_str())
            .map_err(|error| SError::InvalidConfig(error.to_string()))?;
        Ok(())
    }

    pub async fn build(self) -> Result<DnsServer, SError> {
        let server_name = rustls::pki_types::ServerName::try_from(self.server_name)
            .map_err(|error| SError::InvalidConfig(error.to_string()))?;
        let backend = Backend::Tls {
            upstream: self.upstream,
            server_name,
            connector: tls_connector()?,
        };
        DnsServer::new(self.tag, self.bind_addr, backend).await
    }
}

/// Local UDP/TCP DNS listener allocating stable synthetic addresses.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct DnsFakeIpServerCfg {
    pub tag: String,
    pub bind_addr: SocketAddr,
}

impl DnsFakeIpServerCfg {
    pub async fn build(self) -> Result<DnsServer, SError> {
        DnsServer::new(self.tag, self.bind_addr, Backend::FakeIp).await
    }
}

/// Local UDP/TCP DNS listener using the operating system resolver.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct DnsSystemServerCfg {
    pub tag: String,
    pub bind_addr: SocketAddr,
}

impl DnsSystemServerCfg {
    pub async fn build(self) -> Result<DnsServer, SError> {
        DnsServer::new(self.tag, self.bind_addr, Backend::System).await
    }
}
