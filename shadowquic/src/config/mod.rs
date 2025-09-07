use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tracing::Level;

use crate::{
    Inbound, Manager, Outbound,
    direct::outbound::DirectOut,
    error::SError,
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
    socks::{inbound::SocksServer, outbound::SocksClient},
};

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
}
impl InboundCfg {
    async fn build_inbound(self) -> Result<Box<dyn Inbound>, SError> {
        let r: Box<dyn Inbound> = match self {
            InboundCfg::Socks(cfg) => Box::new(SocksServer::new(cfg).await?),
            InboundCfg::ShadowQuic(cfg) => Box::new(ShadowQuicServer::new(cfg)?),
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
    Direct(DirectOutCfg),
}

impl OutboundCfg {
    async fn build_outbound(self) -> Result<Box<dyn Outbound>, SError> {
        let r: Box<dyn Outbound> = match self {
            OutboundCfg::Socks(cfg) => Box::new(SocksClient::new(cfg)),
            OutboundCfg::ShadowQuic(cfg) => Box::new(ShadowQuicClient::new(cfg)),
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

/// Shadowquic outbound configuration
///   
/// example:
/// ```yaml
/// addr: "12.34.56.7:1089" # or "[12:ff::ff]:1089" for dualstack
/// password: "12345678"
/// username: "87654321"
/// server-name: "echo.free.beeceptor.com" # must be the same as jls_upstream in server
/// alpn: ["h3"]
/// initial-mtu: 1400
/// congestion-control: bbr
/// zero-rtt: true
/// over-stream: false  # true for udp over stream, false for udp over datagram
/// ```
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", default)]
pub struct ShadowQuicClientCfg {
    /// username, must be the same as the server
    pub username: String,
    /// password, must be the same as the server
    pub password: String,
    /// Shadowquic server address. example: `127.0.0.0.1:443`, `www.server.com:443`, `[ff::f1]:4443`
    pub addr: String,
    /// Server name, must be the same as the server jls_upstream
    /// domain name
    pub server_name: String,
    /// Alpn of tls, default is \["h3"\], must have common element with server
    #[serde(default = "default_alpn")]
    pub alpn: Vec<String>,
    /// Initial mtu, must be larger than min mtu, at least to be 1200.
    /// 1400 is recommended for high packet loss network. default to be 1300
    #[serde(default = "default_initial_mtu")]
    pub initial_mtu: u16,
    /// Congestion control, default to "bbr", supported: "bbr", "new-reno", "cubic"
    #[serde(default = "default_congestion_control")]
    pub congestion_control: CongestionControl,
    /// Set to true to enable zero rtt, default to true
    #[serde(default = "default_zero_rtt")]
    pub zero_rtt: bool,
    /// Transfer udp over stream or over datagram.
    /// If true, use quic stream to send UDP, otherwise use quic datagram
    /// extension, similar to native UDP in TUIC
    #[serde(default = "default_over_stream")]
    pub over_stream: bool,
    #[serde(default = "default_min_mtu")]
    /// Minimum mtu, must be smaller than initial mtu, at least to be 1200.
    /// 1400 is recommended for high packet loss network. default to be 1290
    pub min_mtu: u16,
    /// Keep alive interval in milliseconds
    /// 0 means disable keep alive, should be smaller than 30_000(idle time).
    /// Disabled by default.
    #[serde(default = "default_keep_alive_interval")]
    pub keep_alive_interval: u32,

    /// Android Only. the unix socket path for protecting android socket
    #[cfg(target_os = "android")]
    pub protect_path: Option<PathBuf>,
}

impl Default for ShadowQuicClientCfg {
    fn default() -> Self {
        Self {
            password: Default::default(),
            username: Default::default(),
            addr: Default::default(),
            server_name: Default::default(),
            alpn: Default::default(),
            initial_mtu: default_initial_mtu(),
            congestion_control: Default::default(),
            zero_rtt: Default::default(),
            over_stream: Default::default(),
            min_mtu: default_min_mtu(),
            keep_alive_interval: default_keep_alive_interval(),
            #[cfg(target_os = "android")]
            protect_path: Default::default(),
        }
    }
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
pub fn default_rate_limit() -> u64 {
    u64::MAX
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

/// Configuration of shadowquic inbound
///
/// Example:
/// ```yaml
/// bind-addr: "0.0.0.0:1443"
/// users:
///   - username: "zhangsan"
///     password: "12345678"
/// jls-upstream:
///   addr: "echo.free.beeceptor.com:443" # domain/ip + port, domain must be the same as client.
///   rate-limit: 1000000 # Limiting forwarding rate in unit of bps. optional, default is disabled
/// server-name: "echo.free.beeceptor.com" # must be the same as client
/// alpn: ["h3"]
/// congestion-control: bbr
/// zero-rtt: true
/// ```
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct ShadowQuicServerCfg {
    /// Binding address. e.g. `0.0.0.0:443`, `[::1]:443`
    pub bind_addr: SocketAddr,
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
    /// Congestion control, default to "bbr", supported: "bbr", "new-reno", "cubic"
    #[serde(default = "default_congestion_control")]
    pub congestion_control: CongestionControl,
    /// Initial mtu, must be larger than min mtu, at least to be 1200.
    /// 1400 is recommended for high packet loss network. default to be 1300
    #[serde(default = "default_initial_mtu")]
    pub initial_mtu: u16,
    /// Minimum mtu, must be smaller than initial mtu, at least to be 1200.
    /// 1400 is recommended for high packet loss network. default to be 1290
    #[serde(default = "default_min_mtu")]
    pub min_mtu: u16,
}

/// Jls upstream configuration
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct JlsUpstream {
    /// Jls upstream address, e.g. `codepn.io:443`, `google.com:443`, `127.0.0.1:443`
    pub addr: String,
    /// Maximum rate for JLS forwarding in unit of bps, default is disabled.
    #[serde(default = "default_rate_limit")]
    pub rate_limit: u64,
}

impl Default for JlsUpstream {
    fn default() -> Self {
        Self {
            addr: String::new(),
            rate_limit: u64::MAX,
        }
    }
}
impl Default for ShadowQuicServerCfg {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:443".parse().unwrap(),
            users: Default::default(),
            jls_upstream: Default::default(),
            alpn: Default::default(),
            zero_rtt: Default::default(),
            congestion_control: Default::default(),
            initial_mtu: default_initial_mtu(),
            min_mtu: default_min_mtu(),
            server_name: None,
        }
    }
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
