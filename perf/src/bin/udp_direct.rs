use std::sync::Arc;

use shadowquic::{
    Manager, config::SocksServerCfg, direct::outbound::DirectOut, socks::inbound::SocksServer,
};

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() -> perf::udp::Result<()> {
    let socks = SocksServer::new(SocksServerCfg {
        tag: "inbound".into(),
        default_outbound: None,
        bind_addr: "127.0.0.1:1089".parse()?,
        users: vec![],
    })
    .await?;
    let manager = Manager::single(Box::new(socks), Arc::new(DirectOut::default()));
    perf::udp::run_managers(vec![manager]).await
}
