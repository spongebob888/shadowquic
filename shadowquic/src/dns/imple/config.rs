use std::{net::SocketAddr, sync::Arc};

use serde::Deserialize;

use super::{Backend, DnsCache, DnsServer, tls_connector};
use crate::error::SError;

/// Local UDP/TCP DNS listener using a UDP upstream, with TCP fallback.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct DnsUdpServerCfg {
    pub tag: String,
    pub bind_addr: SocketAddr,
    /// Literal upstream address avoids recursive bootstrap resolution.
    pub upstream: SocketAddr,
    /// Skip shared DNS cache reads and writes. Defaults to false.
    #[serde(default)]
    pub bypass_cache: bool,
}

impl DnsUdpServerCfg {
    pub async fn build(self) -> Result<DnsServer, SError> {
        self.build_with_cache(Arc::new(DnsCache::default())).await
    }

    pub(crate) async fn build_with_cache(self, cache: Arc<DnsCache>) -> Result<DnsServer, SError> {
        DnsServer::new(
            self.tag,
            self.bind_addr,
            Backend::Udp(self.upstream),
            cache,
            self.bypass_cache,
        )
        .await
    }
}

/// Local UDP/TCP DNS listener using a TCP upstream.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct DnsTcpServerCfg {
    pub tag: String,
    pub bind_addr: SocketAddr,
    pub upstream: SocketAddr,
    /// Skip shared DNS cache reads and writes. Defaults to false.
    #[serde(default)]
    pub bypass_cache: bool,
}

impl DnsTcpServerCfg {
    pub async fn build(self) -> Result<DnsServer, SError> {
        self.build_with_cache(Arc::new(DnsCache::default())).await
    }

    pub(crate) async fn build_with_cache(self, cache: Arc<DnsCache>) -> Result<DnsServer, SError> {
        DnsServer::new(
            self.tag,
            self.bind_addr,
            Backend::Tcp(self.upstream),
            cache,
            self.bypass_cache,
        )
        .await
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
    /// Skip shared DNS cache reads and writes. Defaults to false.
    #[serde(default)]
    pub bypass_cache: bool,
}

impl DnsTlsServerCfg {
    pub fn validate(&self) -> Result<(), SError> {
        rustls_jls::pki_types::ServerName::try_from(self.server_name.as_str())
            .map_err(|error| SError::InvalidConfig(error.to_string()))?;
        Ok(())
    }

    pub async fn build(self) -> Result<DnsServer, SError> {
        self.build_with_cache(Arc::new(DnsCache::default())).await
    }

    pub(crate) async fn build_with_cache(self, cache: Arc<DnsCache>) -> Result<DnsServer, SError> {
        let server_name = rustls_jls::pki_types::ServerName::try_from(self.server_name)
            .map_err(|error| SError::InvalidConfig(error.to_string()))?;
        let backend = Backend::Tls {
            upstream: self.upstream,
            server_name,
            connector: tls_connector()?,
        };
        DnsServer::new(self.tag, self.bind_addr, backend, cache, self.bypass_cache).await
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
        self.build_with_cache(Arc::new(DnsCache::default())).await
    }

    pub(crate) async fn build_with_cache(self, cache: Arc<DnsCache>) -> Result<DnsServer, SError> {
        DnsServer::new(self.tag, self.bind_addr, Backend::FakeIp, cache, true).await
    }
}

/// Local UDP/TCP DNS listener using the operating system resolver.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct DnsSystemServerCfg {
    pub tag: String,
    pub bind_addr: SocketAddr,
    /// Skip shared DNS cache reads and writes. Defaults to false.
    #[serde(default)]
    pub bypass_cache: bool,
}

impl DnsSystemServerCfg {
    pub async fn build(self) -> Result<DnsServer, SError> {
        self.build_with_cache(Arc::new(DnsCache::default())).await
    }

    pub(crate) async fn build_with_cache(self, cache: Arc<DnsCache>) -> Result<DnsServer, SError> {
        DnsServer::new(
            self.tag,
            self.bind_addr,
            Backend::System,
            cache,
            self.bypass_cache,
        )
        .await
    }
}

/// Standalone DNS service configuration, listed under the top-level `dns` key.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(tag = "type")]
pub enum DnsCfg {
    #[serde(rename = "dns-udp")]
    Udp(DnsUdpServerCfg),
    #[serde(rename = "dns-tcp")]
    Tcp(DnsTcpServerCfg),
    #[serde(rename = "dns-tls")]
    Tls(DnsTlsServerCfg),
    #[serde(rename = "dns-fakeip")]
    FakeIp(DnsFakeIpServerCfg),
    #[serde(rename = "dns-system")]
    System(DnsSystemServerCfg),
}

impl DnsCfg {
    pub fn tag(&self) -> &str {
        match self {
            Self::Udp(cfg) => &cfg.tag,
            Self::Tcp(cfg) => &cfg.tag,
            Self::Tls(cfg) => &cfg.tag,
            Self::FakeIp(cfg) => &cfg.tag,
            Self::System(cfg) => &cfg.tag,
        }
    }

    pub fn is_fake_ip(&self) -> bool {
        matches!(self, Self::FakeIp(_))
    }

    pub fn validate(&self) -> Result<(), SError> {
        if let Self::Tls(cfg) = self {
            cfg.validate()?;
        }
        Ok(())
    }

    pub async fn build(self) -> Result<DnsServer, SError> {
        self.build_with_cache(Arc::new(DnsCache::default())).await
    }

    pub(crate) async fn build_with_cache(self, cache: Arc<DnsCache>) -> Result<DnsServer, SError> {
        match self {
            Self::Udp(cfg) => cfg.build_with_cache(cache).await,
            Self::Tcp(cfg) => cfg.build_with_cache(cache).await,
            Self::Tls(cfg) => cfg.build_with_cache(cache).await,
            Self::FakeIp(cfg) => cfg.build_with_cache(cache).await,
            Self::System(cfg) => cfg.build_with_cache(cache).await,
        }
    }
}
