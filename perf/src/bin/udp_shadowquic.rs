use shadowquic::config::{
    AuthUser, CongestionControl, JlsUpstream, ShadowQuicClientCfg, ShadowQuicServerCfg,
    SocksServerCfg, default_initial_mtu,
};

use shadowquic::{
    Manager,
    direct::outbound::DirectOut,
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
    socks::inbound::SocksServer,
};

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
    let sq_client = ShadowQuicClient::new(
        ShadowQuicClientCfg {
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
            ..Default::default()
        },
        std::sync::Arc::new(shadowquic::dns::ResolverManager::new()),
    );

    let client = Manager::single(Box::new(socks_server), std::sync::Arc::new(sq_client));

    let sq_server = ShadowQuicServer::new(ShadowQuicServerCfg {
        tag: "inbound".into(),
        bind_addr: "127.0.0.1:4444".parse().unwrap(),
        users: vec![AuthUser {
            username: "123".into(),
            password: "123".into(),
        }],
        jls_upstream: JlsUpstream {
            addr: "localhost:443".into(),
            ..Default::default()
        },

        alpn: vec!["h3".into()],
        zero_rtt: true,
        initial_mtu: default_initial_mtu(),
        congestion_control: CongestionControl::Bbr,
        ..Default::default()
    })
    .await?;
    let direct_client = DirectOut::default();
    let server = Manager::single(Box::new(sq_server), std::sync::Arc::new(direct_client));

    Ok((client, server))
}
