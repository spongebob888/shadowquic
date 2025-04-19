use std::net::SocketAddr;

use serde::{Deserialize, Serialize};
use tracing::Level;
use educe::Educe;

use crate::{direct::outbound::DirectOut, error::SError, msgs::socks5::SEncode, shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient}, socks::inbound::SocksServer, Inbound, Manager, Outbound};

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    pub inbound: InboundCfg,
    pub outbound: OutboundCfg,
    #[serde(default)]
    pub log_level: LogLevel,
}
impl Config {
    pub async fn build_manager(self) -> Result<Manager,SError> {
        Ok(Manager {
            inbound: self.inbound.build_inbound().await?,
            outbound: self.outbound.build_outbound().await?,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(tag = "type")]
pub enum InboundCfg {
    Socks(SocksServerCfg),
    #[serde(rename = "shadowquic")]
    ShadowQuic(ShadowQuicServerCfg),
}
impl InboundCfg {
    async fn build_inbound(self) -> Result<Box<dyn Inbound>, SError>{
        let r: Box<dyn Inbound> = match self {
            InboundCfg::Socks(cfg) => Box::new(SocksServer::new(cfg).await?),
            InboundCfg::ShadowQuic(cfg) => Box::new(ShadowQuicServer::new(cfg)?),
        };
        Ok(r)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(tag = "type")]
pub enum OutboundCfg {
    Socks(SocksClientCfg),
    #[serde(rename = "shadowquic")]
    ShadowQuic(ShadowQuicClientCfg),
    Direct(DirectOutCfg),
}

impl OutboundCfg {
    async fn build_outbound(self) -> Result<Box<dyn Outbound>, SError>{
        let r: Box<dyn Outbound> = match self {
            OutboundCfg::Socks(cfg) => panic!("Not implemented yet"),
            OutboundCfg::ShadowQuic(cfg) => Box::new(ShadowQuicClient::new(cfg)),
            OutboundCfg::Direct(direct_out_cfg) => Box::new(DirectOut),
        };
        Ok(r)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SocksServerCfg {
    pub bind_addr: SocketAddr,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SocksClientCfg {
    pub addr: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct ShadowQuicClientCfg {
    pub jls_pwd: String,
    pub jls_iv: String,
    pub addr: String,
    pub server_name: String,
    #[serde(default = "default_alpn")]
    pub alpn: Vec<String>,
    #[serde(default = "default_initial_mtu")]
    pub initial_mtu: u16,
    #[serde(default = "default_congestion_control")]
    pub congestion_control: CongestionControl,
    #[serde(default = "default_zero_rtt")]
    pub zero_rtt: bool,
    #[serde(default = "default_over_stream")]
    pub over_stream: bool,
}
fn default_initial_mtu() -> u16 {1400}
fn default_zero_rtt() -> bool {true}
fn default_congestion_control() -> CongestionControl {CongestionControl::Bbr}
fn default_over_stream() -> bool {false}
fn default_alpn() -> Vec<String> {vec!["h3".into()]}

#[derive(Serialize, Deserialize, Default, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum CongestionControl {
    #[default]
    Bbr,
    Cubic,
    NewReno,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct DirectOutCfg;

#[derive(Deserialize)]
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
}

#[derive(Deserialize, Clone, Default)]
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
        let cfg: Config = serde_yaml::from_str(cfgstr).expect("yaml parsed failed");
    }
}
