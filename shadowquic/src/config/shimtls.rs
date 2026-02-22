use serde::Deserialize;
use std::net::SocketAddr;

use crate::{proxy_transform::tls::{inbound::JlsServerCfg, outbound::JlsClientCfg}, proxy_transform::tcp::{inbound::TcpServerCfg, outbound::TcpClientCfg}};


#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ShimTlsClientCfg {
    #[serde(flatten)]
    pub tcp_cfg: TcpClientCfg,
    #[serde(flatten)]
    pub jls_cfg: JlsClientCfg,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ShimTlsServerCfg {
    #[serde(flatten)]
    pub tcp_cfg: TcpServerCfg,
    #[serde(flatten)]
    pub jls_cfg: JlsServerCfg,
}
