use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    pub inbound: InboundCfg,
    pub outbound: OutboundCfg,
    pub log_level: LogLevel,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(tag = "type")]
pub enum InboundCfg {
    Socks(SocksServerCfg),
    ShadowQuic(ShadowQuicServerCfg),
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(tag = "type")]
pub enum OutboundCfg {
    Socks(SocksClientCfg),
    ShadowQuic(ShadowQuicClientCfg),
    Direct(DirectOutCfg),
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

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ShadowQuicClientCfg {
    pub jls_pwd: String,
    pub jls_iv: String,
    pub addr: String,
    pub server_name: String,
    pub alpn: Vec<String>,
    pub initial_mtu: u16,
    pub congestion_control: CongestionControl,
    pub zero_rtt: bool,
    pub over_stream: bool,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[serde(untagged)]
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
    pub alpn: Vec<String>,
    pub zero_rtt: bool,
    pub congestion_control: CongestionControl,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "lowercase")]
#[serde(untagged)]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
    #[serde(alias = "off")]
    Silent,
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
