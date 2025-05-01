use std::net::SocketAddr;

use serde::{Deserialize, Serialize};
use tracing::Level;

use crate::{
    Inbound, Manager, Outbound,
    direct::outbound::DirectOut,
    error::SError,
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
    socks::{inbound::SocksServer, outbound::SocksClient},
};

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
            OutboundCfg::Direct(_) => Box::new(DirectOut),
        };
        Ok(r)
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct SocksServerCfg {
    pub bind_addr: SocketAddr,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct SocksClientCfg {
    pub addr: String,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", default)]
pub struct ShadowQuicClientCfg {
    pub jls_pwd: String,
    pub jls_iv: String,
    pub addr: String,
    /// must be the same as server jls_upstream domain name
    pub server_name: String,
    #[serde(default = "default_alpn")]
    /// default is [h3]
    pub alpn: Vec<String>,
    #[serde(default = "default_initial_mtu")]
    pub initial_mtu: u16,
    #[serde(default = "default_congestion_control")]
    pub congestion_control: CongestionControl,
    #[serde(default = "default_zero_rtt")]
    pub zero_rtt: bool,
    #[serde(default = "default_over_stream")]
    /// if true, use stream to send UDP, otherwise use datagram, similar to native UDP
    /// in TUIC
    pub over_stream: bool,
    #[serde(default = "default_min_mtu")]
    /// minimum mtu, must be smaller than initial mtu, at least to be 1200.
    /// 1400 is recommended for high packet loss network.
    pub min_mtu: u16,
    /// keep alive interval in milliseconds
    /// 0 means disable keep alive, should be smaller than 30_000
    #[serde(default = "default_keep_alive_interval")]
    pub keep_alive_interval: u32,
}

impl Default for ShadowQuicClientCfg {
    fn default() -> Self {
        Self {
            jls_pwd: Default::default(),
            jls_iv: Default::default(),
            addr: Default::default(),
            server_name: Default::default(),
            alpn: Default::default(),
            initial_mtu: default_initial_mtu(),
            congestion_control: Default::default(),
            zero_rtt: Default::default(),
            over_stream: Default::default(),
            min_mtu: default_min_mtu(),
            keep_alive_interval: default_keep_alive_interval(),
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

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub enum CongestionControl {
    #[default]
    Bbr,
    Cubic,
    NewReno,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct DirectOutCfg;

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct ShadowQuicServerCfg {
    pub bind_addr: SocketAddr,
    pub jls_pwd: String,
    pub jls_iv: String,
    pub jls_upstream: String,
    #[serde(default = "default_alpn")]
    pub alpn: Vec<String>,
    #[serde(default = "default_zero_rtt")]
    pub zero_rtt: bool,
    #[serde(default = "default_congestion_control")]
    pub congestion_control: CongestionControl,
    #[serde(default = "default_initial_mtu")]
    pub initial_mtu: u16,
    #[serde(default = "default_min_mtu")]
    pub min_mtu: u16,
}
impl Default for ShadowQuicServerCfg {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:443".parse().unwrap(),
            jls_pwd: Default::default(),
            jls_iv: Default::default(),
            jls_upstream: Default::default(),
            alpn: Default::default(),
            zero_rtt: Default::default(),
            congestion_control: Default::default(),
            initial_mtu: default_initial_mtu(),
            min_mtu: default_min_mtu(),
        }
    }
}
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
"###;
        let _cfg: Config = serde_yaml::from_str(cfgstr).expect("yaml parsed failed");
    }
}
