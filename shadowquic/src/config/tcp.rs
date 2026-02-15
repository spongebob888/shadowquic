use serde::Deserialize;
use std::net::SocketAddr;

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct TlsServerCfg {
    pub bind_addr: SocketAddr,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct TlsClientCfg {
    pub addr: String,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct TcpServerCfg {
    pub bind_addr: SocketAddr,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct TcpClientCfg {
    pub addr: String,
}
