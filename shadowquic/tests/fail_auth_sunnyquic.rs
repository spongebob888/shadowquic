use fast_socks5::client::{Config, Socks5Stream};
use shadowquic::config::{
    AuthUser, CongestionControl, SocksServerCfg, SunnyQuicClientCfg, SunnyQuicServerCfg,
    default_initial_mtu,
};
use shadowquic::sunnyquic::inbound::SunnyQuicServer;
use shadowquic::sunnyquic::outbound::SunnyQuicClient;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use tokio::{net::TcpListener, time::Duration};

use shadowquic::{Manager, direct::outbound::DirectOut, socks::inbound::SocksServer};

use tracing::info;
use tracing::{Level, level_filters::LevelFilter, trace};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const CHUNK_LEN: usize = 1024;
const ROUND: usize = 100;
#[tokio::test]
async fn main() {
    let socks_server = "127.0.0.1:1092";
    let target_addr = "127.0.0.1";
    let target_port = 1445;
    let mut config = Config::default();
    config.set_skip_auth(false);
    test_shadowquic().await;
    tokio::spawn(tcp_peer(1445));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut s = Socks5Stream::connect(socks_server, target_addr.into(), target_port, config)
        .await
        .unwrap();

    let sendbuf: Vec<u8> = (0..(CHUNK_LEN * ROUND)).map(|_| rand::random()).collect();
    let mut recvbuf = vec![0u8; CHUNK_LEN * ROUND];
    // let mut s1:TcpStream = s.get_socket();
    let (mut r, mut w) = s.get_socket_mut().split();
    let fut_1 = async {
        assert!(r.read_exact(&mut recvbuf[0..1]).await.is_err());
    };
    let fut_2 = async {
        assert!(w.write_all(&sendbuf[0..1]).await.is_ok());
    };

    tokio::join!(fut_1, fut_2);
}

async fn test_shadowquic() {
    let filter = tracing_subscriber::filter::Targets::new()
        // Enable the `INFO` level for anything in `my_crate`
        .with_target("tcp", Level::TRACE)
        .with_target("shadowquic", LevelFilter::TRACE);

    // Enable the `DEBUG` level for a specific module.

    // Build a new subscriber with the `fmt` layer using the `Targets`
    // filter we constructed above.
    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .try_init();

    // env_logger::init();
    trace!("Running");

    let socks_server = SocksServer::new(SocksServerCfg {
        bind_addr: "127.0.0.1:1092".parse().unwrap(),
        users: vec![],
    })
    .await
    .unwrap();
    let sq_client = SunnyQuicClient::new(SunnyQuicClientCfg {
        password: "12".into(),
        username: "123".into(),
        addr: "127.0.0.1:4444".parse().unwrap(),
        server_name: "localhost".into(),
        alpn: vec!["h3".into()],
        initial_mtu: 1200,
        congestion_control: CongestionControl::Bbr,
        zero_rtt: true,
        over_stream: false,
        cert_path: Some("../assets/certs/MyCA.pem".into()),
        ..Default::default()
    });

    let client = Manager {
        inbound: Box::new(socks_server),
        outbound: Box::new(sq_client),
    };

    let sq_server = SunnyQuicServer::new(SunnyQuicServerCfg {
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
        cert_path: "../assets/certs/localhost.crt".into(),
        key_path: "../assets/certs/localhost.key".into(),
        ..Default::default()
    })
    .unwrap();
    let direct_client = DirectOut::default();
    let server = Manager {
        inbound: Box::new(sq_server),
        outbound: Box::new(direct_client),
    };

    tokio::spawn(server.run());
    tokio::time::sleep(Duration::from_millis(100)).await;
    tokio::spawn(client.run());
    tokio::time::sleep(Duration::from_millis(100)).await;
}
async fn tcp_peer(port: u16) {
    let lis = TcpListener::bind(("0.0.0.0", port)).await.unwrap();
    let (mut s, _addr) = lis.accept().await.unwrap();
    info!("accepted");
    let (mut r, mut w) = s.split();

    tokio::io::copy(&mut r, &mut w).await.unwrap();
}
