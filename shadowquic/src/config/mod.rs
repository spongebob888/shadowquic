use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tracing::Level;

use crate::{
    Inbound, Manager, Outbound,
    direct::outbound::DirectOut,
    error::SError,
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
    socks::{inbound::SocksServer, outbound::SocksClient},
    sunnyquic::{inbound::SunnyQuicServer, outbound::SunnyQuicClient},
};

mod shadowquic;
mod sunnyquic;
pub use crate::config::shadowquic::*;
pub use crate::config::sunnyquic::*;
#[cfg(target_os = "android")]
use std::path::PathBuf;

/// Overall configuration of shadowquic.
///
/// Example:
/// ```yaml
/// inbound:
///   type: xxx
///   xxx: xxx
/// outbound:
///   type: xxx
///   xxx: xxx
/// log-level: trace # or debug, info, warn, error
/// ```
/// Supported inbound types are listed in [`InboundCfg`]
///
/// Supported outbound types are listed in [`OutboundCfg`]
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    pub inbound: InboundCfg,
    pub outbound: OutboundCfg,
    #[serde(default)]
    pub log_level: LogLevel,
}
impl Config {
    pub async fn build_manager(self) -> Result<Manager, SError> {
        Ok(Manager {
            inbound: self.inbound.build_inbound().await?,
            outbound: self.outbound.build_outbound().await?,
        })
    }
}

/// Inbound configuration
/// example:
/// ```yaml
/// type: socks # or shadowquic
/// bind-addr: "0.0.0.0:443" # "[::]:443"
/// xxx: xxx # other field depending on type
/// ```
/// See [`SocksServerCfg`] and [`ShadowQuicServerCfg`] for configuration field of corresponding type
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
#[serde(tag = "type")]
pub enum InboundCfg {
    Socks(SocksServerCfg),
    #[serde(rename = "shadowquic")]
    ShadowQuic(ShadowQuicServerCfg),
    #[serde(rename = "sunnyquic")]
    SunnyQuic(SunnyQuicServerCfg),
}
impl InboundCfg {
    async fn build_inbound(self) -> Result<Box<dyn Inbound>, SError> {
        let r: Box<dyn Inbound> = match self {
            InboundCfg::Socks(cfg) => Box::new(SocksServer::new(cfg).await?),
            InboundCfg::ShadowQuic(cfg) => Box::new(ShadowQuicServer::new(cfg)?),
            InboundCfg::SunnyQuic(cfg) => Box::new(SunnyQuicServer::new(cfg)?),
        };
        Ok(r)
    }
}

/// Outbound configuration
/// example:
/// ```yaml
/// type: socks # or shadowquic or direct
/// addr: "127.0.0.1:443" # "[::1]:443"
/// xxx: xxx # other field depending on type
/// ```
/// See [`SocksClientCfg`] and [`ShadowQuicClientCfg`] for configuration field of corresponding type
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
#[serde(tag = "type")]
pub enum OutboundCfg {
    Socks(SocksClientCfg),
    #[serde(rename = "shadowquic")]
    ShadowQuic(ShadowQuicClientCfg),
    #[serde(rename = "sunnyquic")]
    SunnyQuic(SunnyQuicClientCfg),
    Direct(DirectOutCfg),
}

impl OutboundCfg {
    async fn build_outbound(self) -> Result<Box<dyn Outbound>, SError> {
        let r: Box<dyn Outbound> = match self {
            OutboundCfg::Socks(cfg) => Box::new(SocksClient::new(cfg)),
            OutboundCfg::ShadowQuic(cfg) => Box::new(ShadowQuicClient::new(cfg)),
            OutboundCfg::SunnyQuic(cfg) => Box::new(SunnyQuicClient::new(cfg)),
            OutboundCfg::Direct(cfg) => Box::new(DirectOut::new(cfg)),
        };
        Ok(r)
    }
}

/// Socks inbound configuration
///
/// Example:
/// ```yaml
/// bind-addr: "0.0.0.0:1089" # or "[::]:1089" for dualstack
/// users:
///  - username: "username"
///    password: "password"
/// ```
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct SocksServerCfg {
    /// Server binding address. e.g. `0.0.0.0:1089`, `[::1]:1089`
    pub bind_addr: SocketAddr,
    /// Socks5 username, optional
    /// Left empty to disable authentication
    #[serde(default = "Vec::new")]
    pub users: Vec<AuthUser>,
}

/// user authentication
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct AuthUser {
    pub username: String,
    pub password: String,
}

/// Socks outbound configuration
/// Example:
/// ```yaml
/// addr: "12.34.56.7:1089" # or "[12:ff::ff]:1089" for dualstack
/// ```
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct SocksClientCfg {
    pub addr: String,
    /// SOCKS5 username, optional
    pub username: Option<String>,
    /// SOCKS5 password, optional
    pub password: Option<String>,
}

pub fn default_initial_mtu() -> u16 {
    1300
}
pub fn default_min_mtu() -> u16 {
    1290
}
pub fn default_zero_rtt() -> bool {
    true
}
pub fn default_congestion_control() -> CongestionControl {
    CongestionControl::Bbr
}
pub fn default_over_stream() -> bool {
    false
}
pub fn default_alpn() -> Vec<String> {
    vec!["h3".into()]
}
pub fn default_keep_alive_interval() -> u32 {
    0
}

pub fn default_gso() -> bool {
    true
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub enum CongestionControl {
    #[default]
    Bbr,
    Cubic,
    NewReno,
}
/// Configuration of direct outbound
/// Example:
/// ```yaml
/// dns-strategy: prefer-ipv4 # or prefer-ipv6, ipv4-only, ipv6-only
/// ```
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "kebab-case")]
pub struct DirectOutCfg {
    #[serde(default)]
    pub dns_strategy: DnsStrategy,
}
/// DNS resolution strategy
/// Default is `prefer-ipv4``
/// - `prefer-ipv4`: try to use ipv4 first, if no ipv4 address, use ipv6
/// - `prefer-ipv6`: try to use ipv6 first, if no ipv6 address, use ipv4
/// - `ipv4-only`: only use ipv4 address
/// - `ipv6-only`: only use ipv6 address
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DnsStrategy {
    #[default]
    PreferIpv4,
    PreferIpv6,
    Ipv4Only,
    Ipv6Only,
}

/// Log level of shadowquic
/// Default level is info.
#[derive(Deserialize, Clone, Default, Debug)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}
impl LogLevel {
    pub fn as_tracing_level(&self) -> Level {
        match self {
            LogLevel::Trace => Level::TRACE,
            LogLevel::Debug => Level::DEBUG,
            LogLevel::Info => Level::INFO,
            LogLevel::Warn => Level::WARN,
            LogLevel::Error => Level::ERROR,
        }
    }
}

#[cfg(test)]
mod test {
    use super::Config;
    #[test]
    fn test() {
        let cfgstr = r###"
inbound:
    type: socks
    bind-addr: 127.0.0.1:1089
outbound:
    type: direct
    dns-strategy: prefer-ipv4
"###;
        let _cfg: Config = serde_yaml::from_str(cfgstr).expect("yaml parsed failed");
    }
}
