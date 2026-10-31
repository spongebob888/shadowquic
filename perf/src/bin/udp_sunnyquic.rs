use shadowquic::config::{
    AuthUser, CongestionControl, SocksServerCfg, SunnyQuicClientCfg, SunnyQuicServerCfg,
    default_initial_mtu,
};
use shadowquic::sunnyquic::inbound::SunnyQuicServer;
use shadowquic::sunnyquic::outbound::SunnyQuicClient;

use shadowquic::{Manager, direct::outbound::DirectOut, socks::inbound::SocksServer};

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() -> perf::udp::Result<()> {
    let (client, server) = setup().await?;
    perf::udp::run(client, server).await
}

async fn setup() -> perf::udp::Result<(Manager, Manager)> {
    let socks_server = SocksServer::new(SocksServerCfg {
        tag: "inbound".into(),
        default_outbound: None,
        bind_addr: "127.0.0.1:1089".parse().unwrap(),
        users: vec![],
    })
    .await?;
    let sq_client = SunnyQuicClient::new(
        SunnyQuicClientCfg {
            tag: "outbound".into(),
            password: "123".into(),
            username: "123".into(),
            addr: "127.0.0.1:4444".parse().unwrap(),
            server_name: "localhost".into(),
            alpn: vec!["h3".into()],
            initial_mtu: 1200,
            congestion_control: CongestionControl::Bbr,
            zero_rtt: true,
            over_stream: false,
            cert_path: Some("./assets/certs/MyCA.pem".into()),
            ..Default::default()
        },
        std::sync::Arc::new(shadowquic::dns::ResolverManager::new()),
    );

    let client = Manager::single(Box::new(socks_server), std::sync::Arc::new(sq_client));

    let sq_server = SunnyQuicServer::new(SunnyQuicServerCfg {
        tag: "inbound".into(),
        bind_addr: "127.0.0.1:4444".parse().unwrap(),
        users: vec![AuthUser {
            username: "123".into(),
            password: "123".into(),
        }],

        alpn: vec!["h3".into()],
        zero_rtt: true,
        initial_mtu: default_initial_mtu(),
        congestion_control: CongestionControl::Bbr,
        server_name: "localhost".into(),
        cert_path: "./assets/certs/localhost.crt".into(),
        key_path: "./assets/certs/localhost.key".into(),
        ..Default::default()
    })
    .await?;
    let direct_client = DirectOut::default();
    let server = Manager::single(Box::new(sq_server), std::sync::Arc::new(direct_client));

    Ok((client, server))
}
